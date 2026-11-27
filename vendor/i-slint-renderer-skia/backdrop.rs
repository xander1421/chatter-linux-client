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

// Optics adapted to SkSL from rust-glass (glass.wgsl and material.rs).
// Copyright (c) 2026 Mohammad Amara. Upstream: Apache-2.0 OR MIT.
// Used under MIT, reproduced in NOTICE-rust-glass.txt.
// https://github.com/MasterKPG/rust-glass/tree/f0b1e9acf9245bb51bbe1883b26711e7fa5eb7a7
const LENS_SOURCE: &str = r#"
uniform shader source;
uniform float2 size;
uniform float radius;
uniform float strength;
uniform float bevelWidth;
uniform float pinch;

const float DISPERSION = 0.20;
const float2 LIGHT = float2(-0.6, -0.8);

float bevel(float depth, float width) {
    float u = 1.0 - min(depth / width, 1.0);
    return 1.0 - sqrt(max(1.0 - u * u, 0.0));
}

float lensShift(float depth) {
    float inner = bevel(depth, bevelWidth);
    float outer = bevel(depth, max(bevelWidth * 0.25, 1.0));
    float shift = strength * bevelWidth * (0.32 * outer - 1.7 * inner);
    // The raised shoulder is reserved for strong, clear lenses.
    if (pinch > 0.0) {
        float shoulder = sin(3.14159265 * min(depth / bevelWidth, 1.0));
        shift += pinch * shoulder * shoulder;
    }
    return shift;
}

half4 main(float2 p) {
    // Use the same rounded rectangle as the Slint clip, so the light and
    // compressed image follow the visible outline even on wide bars.
    float2 halfSize = size * 0.5;
    float2 center = p - halfSize;
    float2 inset = halfSize - abs(center);
    float2 corner = radius - inset;
    bool curved = corner.x > 0.0 && corner.y > 0.0;
    float cornerLength = curved ? length(corner) : 0.0;
    float depth = max(curved ? radius - cornerLength : min(inset.x, inset.y), 0.0);
    float2 lo = float2(0.5);
    float2 hi = max(size - 0.5, lo);
    float2 at = p;
    float2 normal = float2(0.0);
    float facing = 0.0;
    float shift = 0.0;
    if (depth < bevelWidth) {
        if (curved) {
            normal = sign(center) * corner / cornerLength;
        } else {
            normal = corner.x > corner.y ? float2(sign(center.x), 0.0)
                                         : float2(0.0, sign(center.y));
        }
        facing = dot(normal, LIGHT);
        // A steep inward bend compresses the scene, then mirrors it at the
        // outline. The shallower opposing ring softens that bend at the lip.
        shift = lensShift(depth);
        at = p + normal * shift;
    }
    half4 color = source.eval(clamp(at, lo, hi));
    if (depth >= bevelWidth) {
        return color;
    }
    float near = max(facing, 0.0);
    float far = max(-facing, 0.0);
    // Dispersion reverses where the light leaves the body.
    float spread = shift * DISPERSION * mix(1.0, -0.55, smoothstep(0.0, 0.5, far));
    if (abs(spread) > 0.35) {
        float2 split = normal * spread;
        color.r = source.eval(clamp(at + split, lo, hi)).r;
        color.b = source.eval(clamp(at - split, lo, hi)).b;
    }

    // Light crossing the bevel darkens the near wall and fills the far
    // wall. The reflected shadow carries the hue of the refracted scene.
    half body = half(1.0 - smoothstep(0.0, bevelWidth, depth));
    half shade = half(0.18 * near) * body * body;
    half3 transmitted = min(color.rgb * color.rgb * 0.45, half3(color.a));
    color.rgb = mix(color.rgb, transmitted, shade);
    color.rgb = mix(color.rgb, half3(color.a), half(0.10 * far) * body * body);

    // The near and far arcs catch light at different angles. A weak base
    // keeps the rim continuous on the sides perpendicular to the light.
    float line = (depth - 0.8) * 0.49;
    float profile = max(1.0 - line * line, 0.0);
    profile = profile * profile * profile;
    float direction = 0.8 * near * near + far;
    half rim = half(0.54 * mix(0.14, 1.0, direction) * profile);
    half3 reflected = mix(half3(color.a), min(color.rgb * 1.5 + color.a * 0.15,
                                             half3(color.a)), 0.25);
    color.rgb = min(mix(color.rgb, reflected, rim), half3(color.a));
    return color;
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
        // Cubic B-spline reconstruction smooths mid-radius blur. At quarter
        // resolution the broad Gaussian already removes that detail, so keep
        // the cheaper linear reconstruction for the wide chrome surfaces.
        let sampling: skia_safe::SamplingOptions = if scale == 0.5 {
            skia_safe::CubicResampler { b: 1.0, c: 0.0 }.into()
        } else {
            skia_safe::FilterMode::Linear.into()
        };
        image_filters::matrix_transform(
            &skia_safe::Matrix::scale((1.0 / scale, 1.0 / scale)),
            sampling,
            blurred,
        )
    } else {
        // Small blurs keep full-resolution detail.
        image_filters::blur((blur, blur), skia_safe::TileMode::Clamp, None, bounds)
    };
    // Saturation 1 + brightness 0 is an identity colour matrix: skip the pass
    // and its intermediate texture. Keep it only when it is the one stage that
    // materialises the backdrop source for a refraction-only filter (blur
    // produced nothing).
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
                builder
                    .set_uniform_float("size", &[bounds.width(), bounds.height()])
                    .ok()?;
                let strength = refraction.clamp(0.0, 1.0);
                let half_short_side = bounds.width().min(bounds.height()) * 0.5;
                let bevel_width = (half_short_side * 0.22).clamp(2.0, 24.0);
                let pinch = (strength - 0.60).max(0.0) * bevel_width * 0.28;
                builder
                    .set_uniform_float("radius", &[radius.clamp(0.0, half_short_side)])
                    .ok()?;
                builder.set_uniform_float("strength", &[strength]).ok()?;
                builder
                    .set_uniform_float("bevelWidth", &[bevel_width])
                    .ok()?;
                builder.set_uniform_float("pinch", &[pinch]).ok()?;
                // Include displaced samples even when Skia is filtering a
                // partial damage region. The inset clip remains item-local.
                let sample_radius = 2.04 * strength * bevel_width + pinch;
                result = image_filters::runtime_shader_with_options(
                    &builder,
                    sample_radius,
                    "source",
                    result,
                    false,
                );
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

    fn render_lens(draw: impl FnOnce(&skia_safe::Canvas)) -> Vec<u8> {
        let lens = filter(Rect::from_wh(128.0, 128.0), 0.0, 1.0, 0.0, 0.8, 28.0).unwrap();
        render_backdrop(&lens, draw)
    }

    fn render_backdrop(filter: &ImageFilter, draw: impl FnOnce(&skia_safe::Canvas)) -> Vec<u8> {
        let mut surface = skia_safe::surfaces::raster_n32_premul((128, 128)).unwrap();
        let bounds = Rect::from_wh(128.0, 128.0);
        let canvas = surface.canvas();
        draw(canvas);
        canvas.save_layer(
            &skia_safe::canvas::SaveLayerRec::default()
                .bounds(&bounds)
                .backdrop(filter)
                .backdrop_tile_mode(skia_safe::TileMode::Clamp),
        );
        canvas.restore();
        let mut pixels = vec![0; 128 * 128 * 4];
        let info = skia_safe::ImageInfo::new(
            (128, 128),
            skia_safe::ColorType::RGBA8888,
            skia_safe::AlphaType::Unpremul,
            None,
        );
        assert!(surface.read_pixels(&info, &mut pixels, 128 * 4, (0, 0)));
        pixels
    }

    #[test]
    fn dispersion_reverses_on_the_far_wall() {
        let pixels = render_lens(|canvas| {
            for x in 0..128 {
                let mut paint = skia_safe::Paint::default();
                let gray = x as u8 * 2;
                paint.set_color(skia_safe::Color::from_rgb(gray, gray, gray));
                canvas.draw_rect(Rect::from_xywh(x as f32, 0.0, 1.0, 128.0), &paint);
            }
        });
        // The ramp grows brighter from left to right. An inward shift
        // changes sign between the two walls, so a reversing dispersion
        // puts the brighter red sample on both sides.
        for x in [1, 126] {
            let i = (64 * 128 + x) * 4;
            assert!(
                pixels[i] > pixels[i + 2],
                "red should sample farther right at x={x}"
            );
        }
    }

    #[test]
    fn directional_rim_is_brighter_on_the_far_wall() {
        let pixels = render_lens(|canvas| {
            canvas.clear(skia_safe::Color::BLACK);
        });
        let red = |x: usize, y: usize| pixels[(y * 128 + x) * 4];
        assert!(red(64, 127) > red(64, 0));
        assert!(red(64, 127) > red(127, 64) + 10);
        assert_eq!(red(64, 64), 0);
    }

    #[test]
    fn bevel_shadow_carries_the_color_of_transmitted_light() {
        let pixels = render_lens(|canvas| {
            canvas.clear(skia_safe::Color::from_rgb(190, 95, 40));
        });
        let near = &pixels[(5 * 128 + 64) * 4..][..4];
        assert!(near[0] < 190 && near[1] < 95);
        assert!(u32::from(near[0]) * 95 > u32::from(near[1]) * 190);
        assert_eq!(near[3], 255);
    }

    #[test]
    fn quarter_circle_bevel_folds_at_the_lip_and_can_add_a_shoulder() {
        // Exercise the actual SkSL profile without the light or backdrop.
        let (profile, _) = LENS_SOURCE.split_once("half4 main").unwrap();
        let source = format!(
            "{profile}\nhalf4 main(float2 p) {{ return half4(0.5 + lensShift(p.x) / 64.0, 0, 0, 1); }}"
        );
        let effect = RuntimeEffect::make_for_shader(&source, None).unwrap();
        let mut builder = RuntimeShaderBuilder::new(effect);
        builder.set_uniform_float("bevelWidth", &[14.0]).unwrap();
        builder.set_uniform_float("strength", &[0.8]).unwrap();
        let mut surface = skia_safe::surfaces::raster_n32_premul((16, 1)).unwrap();
        let mut paint = skia_safe::Paint::default();
        let info = skia_safe::ImageInfo::new(
            (16, 1),
            skia_safe::ColorType::RGBA8888,
            skia_safe::AlphaType::Unpremul,
            None,
        );
        let mut pixels = [0; 16 * 4];
        let mut draw_profile = |builder: &RuntimeShaderBuilder| {
            paint.set_shader(
                builder
                    .make_shader(&skia_safe::Matrix::new_identity())
                    .unwrap(),
            );
            surface.canvas().draw_paint(&paint);
            assert!(surface.read_pixels(&info, &mut pixels, 16 * 4, (0, 0)));
            pixels
        };
        builder.set_uniform_float("pinch", &[0.0]).unwrap();
        let plain = draw_profile(&builder);
        builder.set_uniform_float("pinch", &[2.0]).unwrap();
        let raised = draw_profile(&builder);
        // Far inside the lens shift is zero, and at the lip it's inward.
        assert_eq!(plain[15 * 4], 128);
        assert!(plain[0] < plain[4] && plain[4] < plain[15 * 4]);
        // The steep lip folds the image: a pixel nearer the edge samples
        // farther inward than its immediate neighbor.
        assert!(i32::from(plain[4]) - i32::from(plain[0]) > 4);
        let u: f32 = 1.0 - 0.5 / 14.0;
        let inner_only = -0.8 * 14.0 * 1.7 * (1.0 - (1.0 - u * u).sqrt());
        let inner_only_pixel = (0.5 + inner_only / 64.0) * 255.0;
        assert!(f32::from(plain[0]) > inner_only_pixel + 5.0);
        assert!(raised[7 * 4] > plain[7 * 4] + 5);
        assert_eq!(raised[15 * 4], plain[15 * 4]);
    }

    #[test]
    fn half_resolution_blur_reconstructs_a_cubic_edge() {
        let bounds = Rect::from_wh(128.0, 128.0);
        let draw_step = |canvas: &skia_safe::Canvas| {
            canvas.clear(skia_safe::Color::BLACK);
            let mut paint = skia_safe::Paint::default();
            paint.set_color(skia_safe::Color::WHITE);
            canvas.draw_rect(Rect::from_xywh(64.0, 0.0, 64.0, 128.0), &paint);
        };
        let blur = filter(bounds, 8.0, 1.0, 0.0, 0.0, 0.0).unwrap();
        let down = image_filters::matrix_transform(
            &skia_safe::Matrix::scale((0.5, 0.5)),
            skia_safe::FilterMode::Linear,
            None,
        );
        let low = image_filters::blur(
            (4.0, 4.0),
            skia_safe::TileMode::Clamp,
            down,
            Rect::from_wh(64.0, 64.0),
        );
        let linear = image_filters::matrix_transform(
            &skia_safe::Matrix::scale((2.0, 2.0)),
            skia_safe::FilterMode::Linear,
            low,
        )
        .unwrap();
        let cubic_pixels = render_backdrop(&blur, draw_step);
        let linear_pixels = render_backdrop(&linear, draw_step);
        let red = |x: usize| (64 * 128 + x) * 4;
        assert!((56..72).all(|x| cubic_pixels[red(x)] < cubic_pixels[red(x + 1)]));
        assert!((48..80).any(|x| cubic_pixels[red(x)] != linear_pixels[red(x)]));
    }

    #[test]
    fn vibrance_lift_increases_color_separation() {
        let backdrop_color = skia_safe::Color::from_rgb(160, 100, 60);
        let color = filter(Rect::from_wh(128.0, 128.0), 0.0, 1.08, 0.0, 0.0, 0.0).unwrap();
        let pixels = render_backdrop(&color, |canvas| {
            canvas.clear(backdrop_color);
        });
        assert!(pixels[0] > 160 && pixels[2] < 60);
        assert_eq!(pixels[3], 255);
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
