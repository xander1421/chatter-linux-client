// Copyright © Chatter contributors
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

//! Backdrop filters operate in the physical, item-local coordinate space.
use skia_safe::runtime_effect::RuntimeShaderBuilder;
use skia_safe::{ImageFilter, Rect, RuntimeEffect, image_filters};
use std::cell::{OnceCell, RefCell};
use std::collections::VecDeque;
use std::sync::OnceLock;

// RuntimeEffect is not Sync. Cache it once on the UI/render thread, never behind
// an unsafe global; the process-wide environment switch contains only a bool.
thread_local! {
    static FILTERS: RefCell<VecDeque<([u32; 9], ImageFilter)>> = const { RefCell::new(VecDeque::new()) };
    static LENS: OnceCell<Result<RuntimeEffect, String>> = const { OnceCell::new() };
}
static ENABLED: OnceLock<bool> = OnceLock::new();

pub(crate) fn enabled() -> bool {
    *ENABLED.get_or_init(|| {
        !matches!(std::env::var("SLINT_BACKDROP_EFFECTS").as_deref(), Ok("0" | "false" | "off"))
    })
}

// Apple "Liquid Glass" optical model ported to SkSL. A continuous-curvature
// squircle (superellipse, n=4) defines the panel; its gradient gives the edge
// normal; the bevel ring near the edge lenses (magnifies) the backdrop inward
// along that normal, splits R/B for chromatic dispersion, and lifts a
// Schlick-Fresnel specular rim toward a fixed key light. All effects fall to
// zero in the flat interior, so the centre samples the backdrop unchanged and
// refraction never needs pixels outside the layer. Displacement/dispersion
// scale with `strength` (Materials.refraction); the rim is intrinsic so glass
// reads as glass even at low refraction. Tuned on-device.
const LENS_SOURCE: &str = r#"
uniform shader source;
uniform float2 size;
uniform float radius;
uniform float strength;

const float DISPERSION = 0.6;

half4 main(float2 p) {
    float2 hs = size * 0.5;
    float2 c = p - hs;
    float2 denom = max(hs, float2(1.0));
    float2 q = abs(c) / denom;
    // Superellipse (n=4) distance without pow: q^4 via mults, ^(1/4) via sqrt.
    // 1 on the edge, <1 inside. n=4 is the Apple continuous-curvature corner.
    float2 q2 = q * q;
    float e = sqrt(sqrt(q2.x * q2.x + q2.y * q2.y));
    float minHalf = min(hs.x, hs.y);
    float sdf = (e - 1.0) * minHalf;                 // <0 inside, 0 on the edge
    // Bevel ring: 1 at the visible edge, 0 in the flat interior, squared so the
    // lensing/dispersion stay at the very edge.
    float bevelW = max(radius, 12.0);
    float bevel = smoothstep(-bevelW, 0.0, sdf);
    float edge = bevel * bevel;
    // Flat interior: the lens and dispersion contribute nothing here, so take
    // the backdrop in a single sample and skip the normal, displacement and
    // the 3-tap colour split entirely. This is the bulk of a wide surface.
    if (edge < 0.004) {
        return source.eval(p);
    }
    // Outward edge normal from the superellipse gradient (q^3 via mults).
    float2 gv = sign(c) * (q2 * q) / denom;
    float glen = length(gv);
    float2 grad = glen > 1e-4 ? gv / glen : float2(0.0);
    float2 lo = float2(0.5);
    float2 hi = max(size - 0.5, float2(0.5));
    // Edge lensing: pull the sample inward along the normal, magnifying content
    // toward the rim.
    float2 baseAt = clamp(p - grad * edge * strength * bevelW * 0.6, lo, hi);
    // Cauchy-style dispersion: R bends less, B more, across the normal at the rim.
    float2 off = grad * (DISPERSION * strength * edge * bevelW * 0.25);
    half4 col;
    col.r = source.eval(clamp(baseAt - off, lo, hi)).r;
    col.g = source.eval(baseAt).g;
    col.b = source.eval(clamp(baseAt + off, lo, hi)).b;
    col.a = source.eval(baseAt).a;
    // No shader rim: the thin 1px optical edge is drawn by GlassSurface
    // (Materials.glass-edge). Apple's edge is a crisp hairline, not a glow.
    return col;
}
"#;

// Negative brightness darkens by scaling, not by subtracting: an offset of
// -0.14 clamped every channel of a dark chat (values below ~36/255) to black,
// so dark-mode glass showed nothing of the content behind it. Scaling keeps
// dark content visible and still bounds bright content. Positive values lift.
fn color_matrix(saturation: f32, brightness: f32) -> [f32; 20] {
    let s = saturation.max(0.0);
    let v = brightness.clamp(-1.0, 1.0);
    let k = if v < 0.0 { 1.0 + v } else { 1.0 };
    let lift = v.max(0.0);
    let [r, g, b] = [0.2126, 0.7152, 0.0722].map(|l| l * (1.0 - s) * k);
    let s = s * k;
    [
        r + s,
        g,
        b,
        0.0,
        lift,
        r,
        g + s,
        b,
        0.0,
        lift,
        r,
        g,
        b + s,
        0.0,
        lift,
        0.0,
        0.0,
        0.0,
        1.0,
        0.0,
    ]
}

/// Resolution the blur convolution runs at, as a fraction of physical pixels.
/// The kernel removes detail finer than its radius, so the larger the blur the
/// more aggressively its input can be downsampled with no visible change. Tiers
/// (not a continuous ramp) keep the cache key stable and small blurs crisp.
fn blur_downscale(blur: f32) -> f32 {
    if blur >= 32.0 {
        0.25
    } else if blur >= 4.0 {
        0.5
    } else {
        1.0
    }
}

pub(crate) fn filter(
    bounds: Rect,
    blur: f32,
    saturation: f32,
    brightness: f32,
    refraction: f32,
    radius: f32,
) -> Option<ImageFilter> {
    if !enabled() || (blur <= 0.0 && saturation == 1.0 && brightness == 0.0 && refraction <= 0.0) {
        return None;
    }
    // Geometry is part of the key: crop and lens uniforms are item-local.
    // Bound this cache so animated sizes/strengths cannot retain filters forever.
    let key = [
        bounds.left,
        bounds.top,
        bounds.right,
        bounds.bottom,
        blur,
        saturation,
        brightness,
        refraction,
        radius,
    ]
    .map(f32::to_bits);
    if let Some(filter) = FILTERS.with(|cache| {
        let mut cache = cache.borrow_mut();
        let index = cache.iter().position(|(cached, _)| *cached == key)?;
        let entry = cache.remove(index)?;
        let filter = entry.1.clone();
        cache.push_back(entry);
        Some(filter)
    }) {
        return Some(filter);
    }
    let scale = blur_downscale(blur);
    let mut result = if blur <= 0.0 {
        None
    } else if scale < 1.0 {
        // Blur at reduced resolution, retaining the physical sigma and cropping
        // after upsampling. A Gaussian of radius `blur` destroys detail finer
        // than itself, so the pre-blur downsample is invisible in the result
        // while the convolution runs on `scale^2` of the pixels. The tier
        // deepens with the radius (see `blur_downscale`): chat chrome blurs
        // ~100-126px physical and so take the quarter-resolution path.
        let down = image_filters::matrix_transform(
            &skia_safe::Matrix::scale((scale, scale)),
            skia_safe::FilterMode::Linear,
            None,
        );
        let low_bounds = Rect::new(
            bounds.left * scale,
            bounds.top * scale,
            bounds.right * scale,
            bounds.bottom * scale,
        );
        let blurred = image_filters::blur(
            (blur * scale, blur * scale),
            skia_safe::TileMode::Clamp,
            down,
            low_bounds,
        );
        image_filters::matrix_transform(
            &skia_safe::Matrix::scale((1.0 / scale, 1.0 / scale)),
            skia_safe::FilterMode::Linear,
            blurred,
        )
    } else {
        // Small blurs keep full-resolution detail.
        image_filters::blur((blur, blur), skia_safe::TileMode::Clamp, None, bounds)
    };
    // Saturation 1 + brightness 0 is an identity colour matrix: skip the pass
    // and its intermediate texture. Keep it only when it is the one stage that
    // materialises the backdrop source for a refraction-only filter (blur
    // produced nothing). (Light mode is identity; dark-mode chrome darkens.)
    if saturation != 1.0 || brightness != 0.0 || result.is_none() {
        result = image_filters::color_filter(
            skia_safe::color_filters::matrix_row_major(&color_matrix(saturation, brightness), None),
            result,
            bounds,
        );
    }
    if refraction > 0.0 {
        let effect = LENS.with(|cell| {
            cell.get_or_init(|| {
                let effect = RuntimeEffect::make_for_shader(LENS_SOURCE, None);
                if let Err(error) = &effect {
                    i_slint_core::debug_log!("Backdrop refraction shader unavailable: {error}");
                }
                effect
            })
            .clone()
        });
        match effect {
            Ok(effect) => {
                let mut builder = RuntimeShaderBuilder::new(effect);
                builder.set_uniform_float("size", &[bounds.width(), bounds.height()]).ok()?;
                builder.set_uniform_float("radius", &[radius]).ok()?;
                builder.set_uniform_float("strength", &[refraction.clamp(0.0, 1.0)]).ok()?;
                result = image_filters::runtime_shader(&builder, "source", result);
            }
            // Keep blur/colour working on Skia builds that reject the shader.
            Err(_) => {}
        }
    }
    if let Some(filter) = &result {
        FILTERS.with(|cache| {
            let mut cache = cache.borrow_mut();
            if cache.len() == 32 {
                cache.pop_front();
            }
            cache.push_back((key, filter.clone()));
        });
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_cache_reuses_parameters_and_bounds_animated_entries() {
        FILTERS.with(|cache| cache.borrow_mut().clear());
        let bounds = Rect::from_wh(120.0, 80.0);
        assert!(filter(bounds, 12.0, 1.7, 0.05, 0.4, 20.0).is_some());
        assert!(filter(bounds, 12.0, 1.7, 0.05, 0.4, 20.0).is_some());
        FILTERS.with(|cache| assert_eq!(cache.borrow().len(), 1));
        for width in 121..161 {
            assert!(
                filter(
                    Rect::from_wh(width as f32, 80.0),
                    12.0,
                    1.7,
                    0.05,
                    0.4,
                    20.0
                )
                .is_some()
            );
        }
        FILTERS.with(|cache| {
            let cache = cache.borrow();
            assert_eq!(cache.len(), 32);
            assert_eq!(cache.back().unwrap().0[2], 160.0f32.to_bits());
        });
    }

    #[test]
    fn saturation_preserves_neutrals_and_brightness_preserves_alpha() {
        let matrix = color_matrix(1.8, -0.2);
        for row in matrix[..15].chunks_exact(5) {
            assert!((row[..3].iter().sum::<f32>() - 0.8).abs() < 0.00001);
            assert_eq!(row[4], 0.0);
        }
        let lifted = color_matrix(1.8, 0.1);
        for row in lifted[..15].chunks_exact(5) {
            assert!((row[..3].iter().sum::<f32>() - 1.0).abs() < 0.00001);
            assert_eq!(row[4], 0.1);
        }
        assert_eq!(&matrix[15..], &[0.0, 0.0, 0.0, 1.0, 0.0]);
    }

    #[test]
    fn backdrop_blurs_existing_pixels_and_leaves_foreground_sharp() {
        let mut surface = skia_safe::surfaces::raster_n32_premul((64, 64)).unwrap();
        let canvas = surface.canvas();
        canvas.clear(skia_safe::Color::BLUE);
        let mut paint = skia_safe::Paint::default();
        paint.set_color(skia_safe::Color::RED);
        canvas.draw_rect(Rect::from_xywh(32.0, 0.0, 32.0, 64.0), &paint);
        canvas.save();
        let bounds = Rect::from_xywh(8.0, 8.0, 48.0, 48.0);
        canvas.clip_rrect(skia_safe::RRect::new_rect_xy(bounds, 8.0, 8.0), None, true);
        let blur = filter(Rect::from_wh(64.0, 64.0), 5.0, 1.0, 0.0, 0.0, 8.0).unwrap();
        canvas.save_layer(
            &skia_safe::canvas::SaveLayerRec::default().bounds(&bounds).backdrop(&blur),
        );
        canvas.restore();
        paint.set_color(skia_safe::Color::GREEN);
        canvas.draw_rect(Rect::from_xywh(28.0, 28.0, 8.0, 8.0), &paint);
        canvas.restore();
        let info = skia_safe::ImageInfo::new(
            (64, 64),
            skia_safe::ColorType::RGBA8888,
            skia_safe::AlphaType::Unpremul,
            None,
        );
        let mut pixels = vec![0u8; 64 * 64 * 4];
        assert!(surface.read_pixels(&info, &mut pixels, 64 * 4, (0, 0)));
        let pixel = |x: usize, y: usize| &pixels[(y * 64 + x) * 4..(y * 64 + x + 1) * 4];
        assert_eq!(pixel(31, 2), &[0, 0, 255, 255], "outside glass stays sharp");
        assert!(
            pixel(31, 18)[0] > 20 && pixel(31, 18)[2] > 20,
            "backdrop blends the red/blue edge"
        );
        assert_eq!(pixel(32, 32), &[0, 255, 0, 255], "foreground stays sharp");
        assert_eq!(pixel(8, 8), &[0, 0, 255, 255], "rounded corner is clipped");
    }

    #[test]
    fn translated_refraction_moves_edge_samples_but_preserves_the_center() {
        let mut surface = skia_safe::surfaces::raster_n32_premul((64, 64)).unwrap();
        let info = skia_safe::ImageInfo::new(
            (64, 64),
            skia_safe::ColorType::RGBA8888,
            skia_safe::AlphaType::Unpremul,
            None,
        );
        surface.canvas().clear(skia_safe::Color::BLUE);
        let mut paint = skia_safe::Paint::default();
        paint.set_color(skia_safe::Color::RED);
        for x in (0..64).step_by(4) {
            surface.canvas().draw_rect(Rect::from_xywh(x as f32, 0.0, 2.0, 64.0), &paint);
        }
        let mut before = vec![0u8; 64 * 64 * 4];
        assert!(surface.read_pixels(&info, &mut before, 64 * 4, (0, 0)));
        let canvas = surface.canvas();
        canvas.save();
        canvas.translate((8.0, 8.0));
        let bounds = Rect::from_wh(48.0, 48.0);
        canvas.clip_rrect(
            skia_safe::RRect::new_rect_xy(bounds, 12.0, 12.0),
            None,
            true,
        );
        let lens = filter(bounds, 0.0, 1.0, 0.0, 0.8, 12.0).unwrap();
        canvas.save_layer(
            &skia_safe::canvas::SaveLayerRec::default().bounds(&bounds).backdrop(&lens),
        );
        canvas.restore();
        canvas.restore();
        let mut after = vec![0u8; before.len()];
        assert!(surface.read_pixels(&info, &mut after, 64 * 4, (0, 0)));
        let center = (32 * 64 + 32) * 4;
        assert_eq!(&before[center..center + 4], &after[center..center + 4]);
        assert!(
            (9..16).any(|x| {
                let index = (32 * 64 + x) * 4;
                before[index..index + 4] != after[index..index + 4]
            }),
            "edge lens must actually displace the backdrop"
        );
    }

    #[test]
    fn lens_shader_and_filter_chain_are_valid() {
        RuntimeEffect::make_for_shader(LENS_SOURCE, None).expect("valid rounded glass lens SkSL");
        assert!(filter(Rect::from_wh(120.0, 80.0), 12.0, 1.7, 0.05, 0.4, 20.0).is_some());
    }

    #[test]
    fn blur_downscale_tiers_deepen_with_radius() {
        assert_eq!(blur_downscale(0.0), 1.0);
        assert_eq!(blur_downscale(3.9), 1.0);
        assert_eq!(blur_downscale(4.0), 0.5);
        assert_eq!(blur_downscale(31.9), 0.5);
        assert_eq!(blur_downscale(32.0), 0.25);
        assert_eq!(blur_downscale(126.0), 0.25); // chat chrome physical blur
    }

    // CPU-raster timing benchmark for the glass backdrop pipeline. Not a GPU
    // measurement (absolute numbers differ on-device), but a reproducible way to
    // attribute cost to each stage and catch regressions while tuning the look.
    //   cargo test -p i-slint-renderer-skia --release -- --ignored --nocapture bench_backdrop_apply
    #[test]
    #[ignore = "timing benchmark; run with --release --ignored --nocapture"]
    fn bench_backdrop_apply() {
        fn apply_once(
            surface: &mut skia_safe::Surface,
            bounds: Rect,
            radius: f32,
            filter: &ImageFilter,
        ) {
            let canvas = surface.canvas();
            canvas.save();
            canvas.clip_rrect(skia_safe::RRect::new_rect_xy(bounds, radius, radius), None, true);
            canvas.save_layer(
                &skia_safe::canvas::SaveLayerRec::default().bounds(&bounds).backdrop(filter),
            );
            canvas.restore();
            canvas.restore();
            // Force the raster work to complete before timing the next apply.
            let info = skia_safe::ImageInfo::new(
                (1, 1),
                skia_safe::ColorType::RGBA8888,
                skia_safe::AlphaType::Premul,
                None,
            );
            let mut px = [0u8; 4];
            let _ = surface.read_pixels(&info, &mut px, 4, (0, 0));
        }

        FILTERS.with(|cache| cache.borrow_mut().clear());
        // A phone chat-header strip, physical pixels.
        let (w, h) = (1080i32, 180i32);
        let mut surface = skia_safe::surfaces::raster_n32_premul((w, h)).unwrap();
        {
            let canvas = surface.canvas();
            canvas.clear(skia_safe::Color::from_argb(255, 20, 40, 60));
            let mut paint = skia_safe::Paint::default();
            paint.set_color(skia_safe::Color::from_argb(255, 210, 130, 50));
            for x in (0..w).step_by(24) {
                canvas.draw_rect(Rect::from_xywh(x as f32, 0.0, 10.0, h as f32), &paint);
            }
        }
        let bounds = Rect::from_wh(w as f32, h as f32);
        let radius = 60.0;
        // (label, blur, saturation, brightness, refraction) — chat chrome is the first row.
        let cases: &[(&str, f32, f32, f32, f32)] = &[
            ("liquid-glass full (blur+vibrancy+lens+disp+spec)", 126.0, 1.5, -0.14, 0.4),
            ("blur+vibrancy only", 126.0, 1.5, -0.14, 0.0),
            ("blur only", 126.0, 1.0, 0.0, 0.0),
            ("lens only (no blur)", 0.0, 1.0, 0.0, 0.4),
        ];
        println!("\n-- backdrop apply ({w}x{h}px, radius {radius}) --");
        for (label, blur, sat, bright, refr) in cases {
            FILTERS.with(|cache| cache.borrow_mut().clear());
            let Some(f) = filter(bounds, *blur, *sat, *bright, *refr, radius) else {
                println!("  {label:52} (no filter)");
                continue;
            };
            apply_once(&mut surface, bounds, radius, &f); // warm
            let n = 120;
            let start = std::time::Instant::now();
            for _ in 0..n {
                apply_once(&mut surface, bounds, radius, &f);
            }
            let ms = start.elapsed().as_secs_f64() * 1000.0 / f64::from(n);
            println!("  {label:52} {ms:7.3} ms/apply  (blur downscale {})", blur_downscale(*blur));
        }
    }
}
