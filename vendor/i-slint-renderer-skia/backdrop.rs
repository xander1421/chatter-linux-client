// Copyright © Chatter contributors
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

//! Backdrop filters operate in the physical, item-local coordinate space.
use skia_safe::runtime_effect::RuntimeShaderBuilder;
use skia_safe::{ImageFilter, Rect, RuntimeEffect, image_filters};
use std::cell::OnceCell;
use std::sync::OnceLock;

// RuntimeEffect is not Sync. Cache it once on the UI/render thread, never behind
// an unsafe global; the process-wide environment switch contains only a bool.
thread_local! {
    static LENS: OnceCell<Result<RuntimeEffect, String>> = const { OnceCell::new() };
}
static ENABLED: OnceLock<bool> = OnceLock::new();

pub(crate) fn enabled() -> bool {
    *ENABLED.get_or_init(|| {
        !matches!(std::env::var("SLINT_BACKDROP_EFFECTS").as_deref(), Ok("0" | "false" | "off"))
    })
}

// Rounded-rectangle signed distance and its normal make the lens follow curved corners.
// Displacement points inward, so refraction never needs source pixels outside the layer.
const LENS_SOURCE: &str = r#"
uniform shader source;
uniform float2 size;
uniform float radius;
uniform float strength;
half4 main(float2 p) {
    float2 halfSize = size * 0.5;
    float2 centered = p - halfSize;
    float2 q = abs(centered) - (halfSize - radius);
    float2 outside = max(q, float2(0));
    float distance = length(outside) + min(max(q.x, q.y), 0.0) - radius;
    float2 normal = length(outside) > 0.001
        ? normalize(outside) * sign(centered)
        : (q.x > q.y ? float2(sign(centered.x), 0) : float2(0, sign(centered.y)));
    float band = max(1.0, min(min(halfSize.x, halfSize.y), max(radius, 12.0)));
    float edge = 1.0 - smoothstep(0.0, band, max(-distance, 0.0));
    float2 sampleAt = clamp(p - normal * edge * strength * band * 0.3,
                           float2(0.5), max(size - 0.5, float2(0.5)));
    return source.eval(sampleAt);
}
"#;

fn color_matrix(saturation: f32, brightness: f32) -> [f32; 20] {
    let s = saturation.max(0.0);
    let [r, g, b] = [0.2126, 0.7152, 0.0722].map(|l| l * (1.0 - s));
    let v = brightness.clamp(-1.0, 1.0);
    [r + s, g, b, 0.0, v, r, g + s, b, 0.0, v, r, g, b + s, 0.0, v, 0.0, 0.0, 0.0, 1.0, 0.0]
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
    let mut result = if blur > 0.0 {
        image_filters::blur((blur, blur), skia_safe::TileMode::Clamp, None, bounds)
    } else {
        None
    };
    // An identity matrix is still a valid backdrop source when only refraction is active.
    result = image_filters::color_filter(
        skia_safe::color_filters::matrix_row_major(&color_matrix(saturation, brightness), None),
        result,
        bounds,
    );
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
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saturation_preserves_neutrals_and_brightness_preserves_alpha() {
        let matrix = color_matrix(1.8, -0.2);
        for row in matrix[..15].chunks_exact(5) {
            assert!((row[..3].iter().sum::<f32>() - 1.0).abs() < 0.00001);
            assert_eq!(row[4], -0.2);
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
        canvas.clip_rrect(skia_safe::RRect::new_rect_xy(bounds, 12.0, 12.0), None, true);
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
}
