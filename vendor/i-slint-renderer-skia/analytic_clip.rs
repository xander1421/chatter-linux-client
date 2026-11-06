// Copyright © Chatter contributors
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

//! Circular asymmetric corners without a device-space software coverage bitmap.
//! Only geometry/program state is retained; no content image or pixel cache.
use skia_safe::{Canvas, Matrix, RRect, RuntimeEffect, runtime_effect::RuntimeShaderBuilder};
use std::cell::OnceCell;

thread_local! {
    static EFFECT: OnceCell<Result<RuntimeEffect, String>> = const { OnceCell::new() };
}

const SOURCE: &str = r#"
uniform float4 bounds;
uniform float4 radii;
uniform float pixelWidth;
uniform float4 opaqueX;
uniform float4 opaqueY;
float distanceToEdge(float2 p) {
    float d = max(max(bounds.x - p.x, p.x - bounds.z),
                  max(bounds.y - p.y, p.y - bounds.w));
    if (p.x < bounds.x + radii.x && p.y < bounds.y + radii.x) {
        d = length(p - (bounds.xy + radii.xx)) - radii.x;
    } else if (p.x > bounds.z - radii.y && p.y < bounds.y + radii.y) {
        d = length(p - float2(bounds.z - radii.y, bounds.y + radii.y)) - radii.y;
    } else if (p.x > bounds.z - radii.z && p.y > bounds.w - radii.z) {
        d = length(p - (bounds.zw - radii.zz)) - radii.z;
    } else if (p.x < bounds.x + radii.w && p.y > bounds.w - radii.w) {
        d = length(p - float2(bounds.x + radii.w, bounds.w - radii.w)) - radii.w;
    }
    return d;
}
half4 main(float2 p) {
    // Coherent interior fragments need no curve, division or coverage work.
    if (p.x >= opaqueX.x && p.y >= opaqueX.y && p.x <= opaqueX.z && p.y <= opaqueX.w) {
        return half4(1.0);
    }
    if (p.x >= opaqueY.x && p.y >= opaqueY.y && p.x <= opaqueY.z && p.y <= opaqueY.w) {
        return half4(1.0);
    }
    float coverage = clamp(0.5 - distanceToEdge(p) / pixelWidth, 0.0, 1.0);
    float4 edgeCoverage = clamp(float4(p.x - bounds.x, p.y - bounds.y,
                                      bounds.z - p.x, bounds.w - p.y) / pixelWidth + 0.5,
                               0.0, 1.0);
    float rectCoverage = edgeCoverage.x * edgeCoverage.y * edgeCoverage.z * edgeCoverage.w;
    // The native rectangular clip supplies straight-edge coverage. Divide it
    // out so intersecting the corner shader does not soften the same edge twice.
    return half4(clamp(coverage / max(rectCoverage, 0.000001), 0.0, 1.0));
}
"#;

/// Return false without modifying the canvas for unsupported transforms/radii
/// or shader failures; the caller then uses the ordinary Skia clip.
pub(crate) fn apply(canvas: &Canvas, rect: &RRect) -> bool {
    if !rect.is_complex() || !rect.is_valid() {
        return false;
    }
    let matrix = canvas.local_to_device_as_3x3();
    let scale = matrix.scale_x();
    // Keep the original path for rotation, reflection, perspective and unequal
    // scales. The common DM path is translation plus a positive uniform scale.
    if !matrix.is_scale_translate()
        || !scale.is_finite()
        || scale <= 0.0
        || scale != matrix.scale_y()
    {
        return false;
    }
    let radii = rect.radii_ref();
    if radii.iter().any(|r| r.x != r.y || !r.x.is_finite()) {
        return false;
    }
    let pixel_width = 1.0 / scale;
    if !pixel_width.is_finite() {
        return false;
    }
    // Combining a curve with an existing antialiased crop can use different
    // edge-coverage rules. Preserve Skia's original result there, including
    // curves cut by the viewport. A crop through the straight body is safe: each
    // rounded corner is either fully retained or fully outside the clip.
    let Some(clip) = canvas.local_clip_bounds() else {
        return false;
    };
    if !canvas.is_clip_rect() {
        return false;
    }
    let b = rect.rect();
    let boxes = [
        skia_safe::Rect::new(b.left, b.top, b.left + radii[0].x, b.top + radii[0].x),
        skia_safe::Rect::new(b.right - radii[1].x, b.top, b.right, b.top + radii[1].x),
        skia_safe::Rect::new(
            b.right - radii[2].x,
            b.bottom - radii[2].x,
            b.right,
            b.bottom,
        ),
        skia_safe::Rect::new(b.left, b.bottom - radii[3].x, b.left + radii[3].x, b.bottom),
    ];
    for (corner, radius) in boxes.iter().zip(radii) {
        if radius.x == 0.0 {
            continue;
        }
        let c = corner.with_outset((2.0 * pixel_width, 2.0 * pixel_width));
        let outside = c.right <= clip.left
            || c.bottom <= clip.top
            || c.left >= clip.right
            || c.top >= clip.bottom;
        let inside = c.left >= clip.left
            && c.top >= clip.top
            && c.right <= clip.right
            && c.bottom <= clip.bottom;
        if !outside && !inside {
            return false;
        }
    }
    let shader = EFFECT.with(|slot| {
        let effect = slot
            .get_or_init(|| RuntimeEffect::make_for_shader(SOURCE, None))
            .as_ref()
            .ok()?;
        let mut shader = RuntimeShaderBuilder::new(effect.clone());
        let b = rect.rect();
        shader
            .set_uniform_float("bounds", &[b.left, b.top, b.right, b.bottom])
            .ok()?;
        shader
            .set_uniform_float("radii", &[radii[0].x, radii[1].x, radii[2].x, radii[3].x])
            .ok()?;
        shader
            .set_uniform_float("pixelWidth", &[pixel_width])
            .ok()?;
        shader
            .set_uniform_float(
                "opaqueX",
                &[
                    b.left + radii[0].x.max(radii[3].x) + pixel_width,
                    b.top + pixel_width,
                    b.right - radii[1].x.max(radii[2].x) - pixel_width,
                    b.bottom - pixel_width,
                ],
            )
            .ok()?;
        shader
            .set_uniform_float(
                "opaqueY",
                &[
                    b.left + pixel_width,
                    b.top + radii[0].x.max(radii[1].x) + pixel_width,
                    b.right - pixel_width,
                    b.bottom - radii[2].x.max(radii[3].x) - pixel_width,
                ],
            )
            .ok()?;
        shader.make_shader(&Matrix::new_identity())
    });
    let Some(shader) = shader else {
        return false;
    };
    // Keep the rectangle in Skia's geometric clip stack. Nested tile rectangles
    // can then be intersected/merged normally, rather than multiplying duplicate
    // antialiased edge coverage. Only the corner cut-outs live in the shader.
    canvas.clip_rect(rect.rect(), None, true);
    canvas.clip_shader(shader, None);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use skia_safe::{Color, Paint, Rect, Vector};

    #[test]
    fn asymmetric_clip_preserves_shape_and_antialiased_edges() {
        for scale in [1.0, 3.5] {
            for shift in [0.0, 0.25, 0.75] {
                let rr = RRect::new_rect_radii(
                    Rect::from_xywh(5.0, 5.0, 42.0, 34.0),
                    &[
                        Vector::new(13.0, 13.0),
                        Vector::new(4.0, 4.0),
                        Vector::new(2.0, 2.0),
                        Vector::new(13.0, 13.0),
                    ],
                );
                let render = |analytic| {
                    let mut surface = skia_safe::surfaces::raster_n32_premul((192, 192)).unwrap();
                    let canvas = surface.canvas();
                    canvas.clear(Color::TRANSPARENT);
                    canvas.scale((scale, scale));
                    canvas.translate((shift, shift));
                    if analytic {
                        assert!(apply(canvas, &rr));
                    } else {
                        canvas.clip_rrect(rr, None, true);
                    }
                    let mut paint = Paint::default();
                    paint.set_color(Color::WHITE);
                    canvas.draw_paint(&paint);
                    let mut bytes = vec![0; 192 * 192 * 4];
                    assert!(surface.read_pixels(
                        &skia_safe::ImageInfo::new_n32_premul((192, 192), None),
                        &mut bytes,
                        192 * 4,
                        (0, 0)
                    ));
                    bytes
                };
                let expected = render(false);
                let actual = render(true);
                let errors: Vec<_> = actual
                    .iter()
                    .zip(&expected)
                    .map(|(a, b)| a.abs_diff(*b) as u64)
                    .collect();
                let max = errors.iter().copied().max().unwrap();
                let sum: u64 = errors.iter().sum();
                // CPU clipping quantizes curved-edge coverage differently from
                // Ganesh. Differences must stay within the antialiased fringe,
                // below one quarter of full coverage at any sample.
                assert!(
                    max <= 64 && sum as f64 / errors.len() as f64 <= 0.15,
                    "scale={scale} shift={shift} max={max} mean={}",
                    sum as f64 / errors.len() as f64
                );
                for (a, b) in actual.chunks_exact(4).zip(expected.chunks_exact(4)) {
                    if b[3] == 0 {
                        assert!(a[3] <= 48);
                    }
                    if b[3] == 255 {
                        assert!(a[3] >= 207);
                    }
                }
            }
        }
    }

    #[test]
    fn ordinary_and_unsupported_clips_keep_the_original_path() {
        let mut surface = skia_safe::surfaces::raster_n32_premul((64, 64)).unwrap();
        let canvas = surface.canvas();
        let simple = RRect::new_rect_xy(Rect::from_wh(32., 32.), 4., 4.);
        assert!(!apply(canvas, &simple));
        let complex = RRect::new_rect_radii(
            Rect::from_wh(32., 32.),
            &[
                Vector::new(8., 8.),
                Vector::new(2., 2.),
                Vector::new(4., 4.),
                Vector::new(2., 2.),
            ],
        );
        let before = canvas.local_clip_bounds();
        canvas.scale((1., 2.));
        let scaled = canvas.local_clip_bounds();
        assert!(!apply(canvas, &complex));
        assert_eq!(canvas.local_clip_bounds(), scaled);
        assert_ne!(before, scaled);
        canvas.reset_matrix();
        let inset = RRect::new_rect_radii(Rect::from_xywh(5., 5., 32., 32.), complex.radii_ref());
        canvas.clip_rect(Rect::from_xywh(0., 8., 64., 56.), None, true);
        let before = canvas.local_clip_bounds();
        assert!(
            !apply(canvas, &inset),
            "partially cropped curves must use Skia's existing clip"
        );
        assert_eq!(canvas.local_clip_bounds(), before);
    }
}
