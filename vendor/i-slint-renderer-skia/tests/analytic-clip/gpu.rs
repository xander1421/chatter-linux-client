// Copyright © Chatter contributors
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0
//! Synthetic GPU readback checks; never reads client data or screenshots.
#[path = "../../analytic_clip.rs"]
mod analytic_clip;
#[cfg(target_os = "android")]
mod device {
    use super::analytic_clip;
    use std::ffi::{c_char, c_void};
    #[link(name = "EGL")]
    unsafe extern "C" {
        fn eglGetDisplay(d: *mut c_void) -> *mut c_void;
        fn eglInitialize(d: *mut c_void, major: *mut i32, minor: *mut i32) -> u32;
        fn eglBindAPI(api: u32) -> u32;
        fn eglChooseConfig(
            d: *mut c_void,
            a: *const i32,
            c: *mut *mut c_void,
            n: i32,
            count: *mut i32,
        ) -> u32;
        fn eglCreatePbufferSurface(d: *mut c_void, c: *mut c_void, a: *const i32) -> *mut c_void;
        fn eglCreateContext(
            d: *mut c_void,
            c: *mut c_void,
            share: *mut c_void,
            a: *const i32,
        ) -> *mut c_void;
        fn eglMakeCurrent(
            d: *mut c_void,
            draw: *mut c_void,
            read: *mut c_void,
            c: *mut c_void,
        ) -> u32;
        fn eglGetProcAddress(n: *const c_char) -> *const c_void;
        fn eglDestroySurface(d: *mut c_void, s: *mut c_void) -> u32;
        fn eglDestroyContext(d: *mut c_void, c: *mut c_void) -> u32;
        fn eglTerminate(d: *mut c_void) -> u32;
    }
    pub fn run() {
        unsafe {
            let d = eglGetDisplay(std::ptr::null_mut());
            assert!(!d.is_null());
            assert_ne!(
                eglInitialize(d, std::ptr::null_mut(), std::ptr::null_mut()),
                0
            );
            assert_ne!(eglBindAPI(0x30A0), 0);
            let a = [
                0x3033, 1, 0x3040, 4, 0x3024, 8, 0x3023, 8, 0x3022, 8, 0x3021, 8, 0x3026, 8, 0x3038,
            ];
            let mut cfg = std::ptr::null_mut();
            let mut n = 0;
            assert_ne!(eglChooseConfig(d, a.as_ptr(), &mut cfg, 1, &mut n), 0);
            assert!(n > 0);
            let s = eglCreatePbufferSurface(d, cfg, [0x3057, 256, 0x3056, 256, 0x3038].as_ptr());
            assert!(!s.is_null());
            let c = eglCreateContext(d, cfg, std::ptr::null_mut(), [0x3098, 3, 0x3038].as_ptr());
            assert!(!c.is_null());
            assert_ne!(eglMakeCurrent(d, s, s, c), 0);
            let interface = skia_safe::gpu::gl::Interface::new_load_with_cstr(|n| {
                eglGetProcAddress(n.as_ptr())
            })
            .unwrap();
            let mut ctx = skia_safe::gpu::direct_contexts::make_gl(interface, None).unwrap();
            for nested in [0, 1, 2, 3, 4, 6] {
                for scale in [1.0, 3.5] {
                    for shift in [0., 0.25, 0.75] {
                        let rr = skia_safe::RRect::new_rect_radii(
                            skia_safe::Rect::from_xywh(5., 5., 42., 34.),
                            &[
                                (13., 13.).into(),
                                (4., 4.).into(),
                                (2., 2.).into(),
                                (13., 13.).into(),
                            ],
                        );
                        let mut render = |analytic: bool| {
                            let info = skia_safe::ImageInfo::new_n32_premul((192, 192), None);
                            let mut surf = skia_safe::gpu::surfaces::render_target(
                                &mut ctx,
                                skia_safe::gpu::Budgeted::Yes,
                                &info,
                                0,
                                None,
                                None,
                                None,
                                None,
                            )
                            .unwrap();
                            let cv = surf.canvas();
                            cv.clear(skia_safe::Color::TRANSPARENT);
                            if nested == 3 {
                                cv.clip_rect(
                                    skia_safe::Rect::from_xywh(0., 30., 192., 100.),
                                    None,
                                    true,
                                );
                            }
                            if nested == 4 {
                                cv.clip_rect(
                                    skia_safe::Rect::from_xywh(
                                        0.,
                                        (22. + shift) * scale,
                                        192.,
                                        192.,
                                    ),
                                    None,
                                    true,
                                );
                            }
                            cv.scale((scale, scale));
                            cv.translate((shift, shift));
                            if analytic {
                                let applied = analytic_clip::apply(cv, &rr);
                                assert_eq!(applied, nested != 3);
                                if !applied {
                                    cv.clip_rrect(rr, None, true);
                                }
                            } else {
                                cv.clip_rrect(rr, None, true);
                            }
                            if nested == 1 {
                                cv.clip_rect(rr.rect(), None, true);
                            }
                            if nested == 2 {
                                cv.clip_rect(
                                    skia_safe::Rect::from_xywh(5., 5., 21., 34.),
                                    None,
                                    true,
                                );
                            }
                            let mut paint = skia_safe::Paint::default();
                            paint.set_color(skia_safe::Color::WHITE);
                            if nested == 6 {
                                paint.set_anti_alias(true);
                                if analytic {
                                    cv.draw_rect(rr.rect(), &paint);
                                } else {
                                    cv.draw_rrect(rr, &paint);
                                }
                                paint.set_style(skia_safe::PaintStyle::Stroke);
                                paint.set_stroke_width(1.);
                                paint.set_color(skia_safe::Color::from_argb(64, 128, 128, 128));
                                cv.draw_rrect(rr.with_inset((0.5, 0.5)), &paint);
                            } else {
                                cv.draw_paint(&paint);
                            }
                            let mut bytes = vec![0; 192 * 192 * 4];
                            assert!(surf.read_pixels(&info, &mut bytes, 192 * 4, (0, 0)));
                            bytes
                        };
                        let expected = render(false);
                        let actual = render(true);
                        let mut max = 0;
                        let mut sum = 0u64;
                        let mut changed = 0;
                        let mut outside = 0;
                        let mut inside = 0;
                        for (a, b) in actual.chunks_exact(4).zip(expected.chunks_exact(4)) {
                            let e = a[3].abs_diff(b[3]);
                            max = max.max(e);
                            sum += e as u64;
                            changed += usize::from(e > 0);
                            if b[3] == 0 {
                                outside = outside.max(a[3]);
                            }
                            if b[3] == 255 {
                                inside = inside.max(255 - a[3]);
                            }
                        }
                        println!(
                            "nested={nested} scale={scale} shift={shift} max={max} mean={} changed={changed} outside={outside} inside={inside}",
                            sum as f64 / (192. * 192.)
                        );
                        // Coverage quantization differs at clip intersections; the contour
                        // must stay inside a single physical-pixel antialiasing fringe.
                        if nested == 6 {
                            assert!(max <= 128 && sum as f64 / (192. * 192.) <= 0.75);
                        } else {
                            assert!(max <= 48 && sum as f64 / (192. * 192.) <= 0.15);
                        }
                        for y in 1..191 {
                            for x in 1..191 {
                                let i = (y * 192 + x) * 4 + 3;
                                let a = actual[i] >= 128;
                                let b = expected[i] >= 128;
                                if a != b {
                                    let mut near = false;
                                    for dy in -1isize..=1 {
                                        for dx in -1isize..=1 {
                                            let j = (((y as isize + dy) * 192 + (x as isize + dx))
                                                as usize)
                                                * 4
                                                + 3;
                                            if (expected[j] >= 128) == a {
                                                near = true;
                                            }
                                        }
                                    }
                                    assert!(near, "contour moved more than one device pixel");
                                }
                            }
                        }
                    }
                }
            }
            ctx.flush_and_submit();
            drop(ctx);
            assert_ne!(
                eglMakeCurrent(
                    d,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut()
                ),
                0
            );
            assert_ne!(eglDestroyContext(d, c), 0);
            assert_ne!(eglDestroySurface(d, s), 0);
            assert_ne!(eglTerminate(d), 0);
            println!("SYNTHETIC_GPU_CHECK_COMPLETE");
        }
    }
}
#[cfg(target_os = "android")]
fn main() {
    device::run();
}
#[cfg(not(target_os = "android"))]
fn main() {
    eprintln!("Build for arm64-v8a and run on the Android test device; see README.md");
}
