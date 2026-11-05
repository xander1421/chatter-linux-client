// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

// cSpell: ignore fboid
use std::num::NonZeroU32;
use std::{cell::RefCell, sync::Arc};

use glutin::{
    config::GetGlConfig,
    context::{ContextApi, ContextAttributesBuilder},
    display::GetGlDisplay,
    prelude::*,
    surface::{SurfaceAttributesBuilder, WindowSurface},
};
use i_slint_core::api::{GraphicsAPI, PhysicalSize as PhysicalWindowSize, Window};
use i_slint_core::graphics::{BorrowedOpenGLTexture, RequestedGraphicsAPI, RequestedOpenGLVersion};
use i_slint_core::partial_renderer::DirtyRegion;
use i_slint_core::platform::PlatformError;
use i_slint_core::renderer::DrawOutcome;

use crate::SkiaSharedContext;

// Front-buffer (single-buffer) rendering experiment helpers. `setprop
// debug.chatter.front_buffer 1` before launch makes config selection prefer a
// config that supports single-buffer rendering so the runtime switch can engage.
#[cfg(target_os = "android")]
fn front_buffer_requested() -> bool {
    unsafe extern "C" {
        fn __system_property_get(name: *const std::ffi::c_char, value: *mut std::ffi::c_char) -> i32;
    }
    let mut value = [0u8; 92];
    // SAFETY: fixed property name and full-size writable buffer.
    let length = unsafe {
        __system_property_get(c"debug.chatter.front_buffer".as_ptr(), value.as_mut_ptr().cast())
    };
    length == 1 && value[0] == b'1'
}

#[cfg(target_os = "android")]
fn egl_config_is_mutable(config: &glutin::config::Config) -> bool {
    use glutin::config::{AsRawConfig, RawConfig};
    use glutin::display::{AsRawDisplay, GetGlDisplay, RawDisplay};
    use std::ffi::c_void;
    let display = config.display();
    let (d, c): (*mut c_void, *mut c_void) = match (display.raw_display(), config.raw_config()) {
        (RawDisplay::Egl(d), RawConfig::Egl(c)) => (d.cast_mut(), c.cast_mut()),
        _ => return false,
    };
    // SAFETY: EGL ABI; live display/config owned by glutin; null-checked symbol.
    unsafe {
        let p = display.get_proc_address(c"eglGetConfigAttrib");
        if p.is_null() {
            return false;
        }
        type Get = unsafe extern "C" fn(*mut c_void, *mut c_void, i32, *mut i32) -> u32;
        let get: Get = core::mem::transmute(p);
        let mut val: i32 = 0;
        // EGL_SURFACE_TYPE 0x3033, EGL_MUTABLE_RENDER_BUFFER_BIT_KHR 0x1000.
        if get(d, c, 0x3033, &mut val) == 0 {
            return false;
        }
        (val & 0x1000) != 0
    }
}

/// Wraps a [`glutin::context::PossiblyCurrentContext`] and makes it not-current on drop,
/// so that no stale thread-local state remains after the context is destroyed.
struct GlutinContext(Option<glutin::context::PossiblyCurrentContext>);

impl GlutinContext {
    fn new(context: glutin::context::PossiblyCurrentContext) -> Self {
        Self(Some(context))
    }
}

impl std::ops::Deref for GlutinContext {
    type Target = glutin::context::PossiblyCurrentContext;
    fn deref(&self) -> &Self::Target {
        self.0.as_ref().unwrap()
    }
}

impl Drop for GlutinContext {
    fn drop(&mut self) {
        if let Some(context) = self.0.take()
            && let Err(e) = context.make_not_current()
        {
            i_slint_core::debug_log!(
                "Skia OpenGL Renderer: Failed to make context not current: {e}"
            );
        }
    }
}

/// This surface type renders into the given window with OpenGL, using glutin and glow libraries.
pub struct OpenGLSurface {
    #[cfg(all(target_os = "android", feature = "experimental-swappy"))]
    swappy: Option<crate::android_swappy::Swappy>,
    #[cfg(target_os = "android")]
    frame_timestamps: RefCell<crate::android_frame_timestamps::Timestamps>,
    #[cfg(target_os = "android")]
    gpu_timestamps: Option<RefCell<crate::android_gpu_timestamps::Timestamps>>,
    fb_info: skia_safe::gpu::gl::FramebufferInfo,
    surface: RefCell<skia_safe::Surface>,
    gr_context: RefCell<skia_safe::gpu::DirectContext>,
    glutin_context: GlutinContext,
    glutin_surface: glutin::surface::Surface<glutin::surface::WindowSurface>,
}

impl super::Surface for OpenGLSurface {
    fn new(
        _shared_context: &SkiaSharedContext,
        window_handle: Arc<dyn raw_window_handle::HasWindowHandle + Send + Sync>,
        display_handle: Arc<dyn raw_window_handle::HasDisplayHandle + Send + Sync>,
        size: PhysicalWindowSize,
        requested_graphics_api: Option<RequestedGraphicsAPI>,
    ) -> Result<Self, PlatformError> {
        Self::new_with_config(
            window_handle,
            display_handle,
            size,
            requested_graphics_api.as_ref().map(TryInto::try_into).transpose()?,
            glutin::config::ConfigTemplateBuilder::new(),
            None,
        )
    }

    fn name(&self) -> &'static str {
        "opengl"
    }

    #[cfg(target_os = "android")]
    fn set_android_swap_interval_zero(&self, zero: bool) -> Result<bool, PlatformError> {
        self.ensure_context_current()?;
        let interval = if zero {
            glutin::surface::SwapInterval::DontWait
        } else {
            glutin::surface::SwapInterval::Wait(NonZeroU32::new(1).unwrap())
        };
        Ok(self.glutin_surface.set_swap_interval(&self.glutin_context, interval).is_ok())
    }

    // Front-buffer (single-buffer) rendering experiment. EGL_KHR_mutable_render_buffer
    // lets draws go straight to the scanned-out buffer, collapsing the ~2-frame
    // compositor queue latency to near zero during a drag — at the cost of tearing.
    // Returns whether the driver accepted it; a config without the mutable bit declines.
    #[cfg(target_os = "android")]
    fn set_front_buffer(&self, single: bool) -> bool {
        use glutin::display::{AsRawDisplay, RawDisplay};
        use glutin::surface::{AsRawSurface, RawSurface};
        use std::ffi::c_void;
        if self.ensure_context_current().is_err() {
            return false;
        }
        let display = self.glutin_context.display();
        let (d, s): (*mut c_void, *mut c_void) =
            match (display.raw_display(), self.glutin_surface.raw_surface()) {
                (RawDisplay::Egl(d), RawSurface::Egl(s)) => (d.cast_mut(), s.cast_mut()),
                _ => return false,
            };
        // EGL_RENDER_BUFFER 0x3086, EGL_SINGLE_BUFFER 0x3085, EGL_BACK_BUFFER 0x3084.
        let value = if single { 0x3085 } else { 0x3084 };
        // SAFETY: EGL ABI; live owning display/surface from glutin; null-checked symbol.
        unsafe {
            let p = display.get_proc_address(c"eglSurfaceAttrib");
            if p.is_null() {
                return false;
            }
            type Attrib = unsafe extern "C" fn(*mut c_void, *mut c_void, i32, i32) -> u32;
            let attrib: Attrib = core::mem::transmute(p);
            attrib(d, s, 0x3086, value) != 0
        }
    }

    fn with_graphics_api(&self, callback: &mut dyn FnMut(GraphicsAPI<'_>)) {
        let api = GraphicsAPI::NativeOpenGL {
            get_proc_address: &|name| {
                self.glutin_context.display().get_proc_address(name) as *const _
            },
        };
        callback(api)
    }

    fn with_active_surface(&self, callback: &mut dyn FnMut()) -> Result<(), PlatformError> {
        self.ensure_context_current()?;
        callback();
        Ok(())
    }

    fn render(
        &self,
        _window: &Window,
        size: PhysicalWindowSize,
        callback: &dyn Fn(
            &skia_safe::Canvas,
            Option<&mut skia_safe::gpu::DirectContext>,
            u8,
        ) -> Option<DirtyRegion>,
        pre_present_callback: &RefCell<Option<Box<dyn FnMut()>>>,
    ) -> Result<DrawOutcome, PlatformError> {
        #[cfg(all(target_os = "android", feature = "painted-scroll-probe"))]
        let painted_frame = {
            #[cfg(feature = "experimental-swappy")]
            let ordinary = self.swappy.is_none();
            #[cfg(not(feature = "experimental-swappy"))]
            let ordinary = true;
            let enabled = i_slint_core::painted_scroll_probe::enabled();
            i_slint_core::painted_scroll_probe::FrameGuard::begin_observed(ordinary,
                enabled && crate::renderer_trace::observation_enabled(), crate::renderer_trace::counter)
        };
        #[cfg(all(target_os = "android", feature = "experimental-swappy"))]
        crate::renderer_trace::counter(
            c"Chatter Swappy enabled",
            i64::from(self.swappy.is_some()),
        );
        #[cfg(target_os = "android")]
        let _frame_trace = crate::renderer_trace::Section::new(c"Chatter Skia EGL render");
        #[cfg(target_os = "android")]
        let acquire_trace = crate::renderer_trace::Section::new(c"Chatter Skia EGL acquire");
        if let Err(error) = self.ensure_context_current() {
            #[cfg(feature = "painted-scroll-probe")]
            i_slint_core::painted_scroll_probe::cancel();
            return Err(error);
        }
        #[cfg(all(target_os = "android", feature = "experimental-swappy"))]
        if let Some(swappy) = &self.swappy {
            swappy.record_frame_start();
        } else {
            crate::renderer_trace::counter(c"Chatter Swappy mode", 0);
        }

        let current_context = &self.glutin_context;

        let gr_context = &mut self.gr_context.borrow_mut();

        let mut surface = self.surface.borrow_mut();

        let width = size.width.try_into().ok();
        let height = size.height.try_into().ok();

        if let Some((width, height)) = width.zip(height)
            && (width != surface.width() || height != surface.height())
        {
            *surface = Self::create_internal_surface(
                self.fb_info,
                current_context,
                gr_context,
                width,
                height,
            )?;
        }

        #[cfg(target_os = "android")]
        drop(acquire_trace);
        let skia_canvas = surface.canvas();

        #[cfg(target_os = "android")]
        if let Some(timestamps) = &self.gpu_timestamps {
            timestamps.borrow_mut().begin(&current_context.display());
        }
        skia_canvas.save();
        #[cfg(target_os = "android")]
        let callback_trace = crate::renderer_trace::Section::new(c"Chatter Skia surface callback");
        callback(
            skia_canvas,
            Some(gr_context),
            u8::try_from(self.glutin_surface.buffer_age()).unwrap_or_default(),
        );
        #[cfg(target_os = "android")]
        drop(callback_trace);
        skia_canvas.restore();
        #[cfg(target_os = "android")]
        if let Some(timestamps) = &self.gpu_timestamps {
            timestamps.borrow_mut().end();
        }

        if let Some(pre_present_callback) = pre_present_callback.borrow_mut().as_mut() {
            #[cfg(target_os = "android")]
            let _present_trace = crate::renderer_trace::Section::new(c"Chatter Skia pre present");
            pre_present_callback();
        }

        #[cfg(target_os = "android")]
        let _swap_trace = crate::renderer_trace::Section::new(c"Chatter Skia EGL swap");
        #[cfg(all(target_os = "android", feature = "experimental-swappy"))]
        crate::renderer_trace::counter(c"Chatter Swappy app observer suppressed", i64::from(self.swappy.is_some()));
        #[cfg(all(target_os = "android", feature = "experimental-swappy"))]
        let frame_id = if self.swappy.is_none() {
            self.frame_timestamps.borrow_mut().before_swap(&current_context.display(), &self.glutin_surface)
        } else { None };
        #[cfg(all(target_os = "android", not(feature = "experimental-swappy")))]
        let frame_id = self
            .frame_timestamps
            .borrow_mut()
            .before_swap(&current_context.display(), &self.glutin_surface);
        let result: Result<(), String> = {
            #[cfg(all(target_os = "android", feature = "experimental-swappy"))]
            if let Some(swappy) = &self.swappy {
                swappy
                    .swap(&current_context.display(), &self.glutin_surface)
                    .map_err(str::to_owned)
            } else {
                self.glutin_surface
                    .swap_buffers(current_context)
                    .map_err(|e| e.to_string())
            }
            #[cfg(not(all(
                target_os = "android",
                feature = "experimental-swappy"
            )))]
            self.glutin_surface
                .swap_buffers(current_context)
                .map_err(|e| e.to_string())
        };
        #[cfg(target_os = "android")]
        if result.is_ok() {
            #[cfg(feature = "experimental-swappy")]
            if self.swappy.is_none() {
                self.frame_timestamps.borrow_mut().swapped(frame_id);
            }
            #[cfg(not(feature = "experimental-swappy"))]
            self.frame_timestamps.borrow_mut().swapped(frame_id);
        }
        #[cfg(all(target_os = "android", feature = "painted-scroll-probe"))]
        if i_slint_core::painted_scroll_probe::active() {
            painted_frame.commit(self.frame_timestamps.borrow().painted_generation(), frame_id,
                result.is_ok(), crate::renderer_trace::counter);
        }
        #[cfg(feature = "painted-scroll-probe")]
        if result.is_err() { i_slint_core::painted_scroll_probe::cancel(); }
        result.map(|_| DrawOutcome::Success).map_err(|glutin_error| {
            format!("Skia OpenGL Renderer: Error swapping buffers: {glutin_error}").into()
        })
    }

    fn resize_event(&self, size: PhysicalWindowSize) -> Result<(), PlatformError> {
        self.ensure_context_current()?;

        if let Some((width, height)) = size.width.try_into().ok().zip(size.height.try_into().ok()) {
            self.glutin_surface.resize(&self.glutin_context, width, height);
        }

        Ok(())
    }

    fn bits_per_pixel(&self) -> Result<u8, PlatformError> {
        let config = self.glutin_context.config();
        let rgb_bits = match config.color_buffer_type() {
            Some(glutin::config::ColorBufferType::Rgb { r_size, g_size, b_size }) => {
                r_size + g_size + b_size
            }
            other => {
                return Err(format!(
                    "Skia OpenGL Renderer: unsupported color buffer {other:?} encountered"
                )
                .into());
            }
        };
        Ok(rgb_bits + config.alpha_size())
    }

    fn import_opengl_texture(
        &self,
        canvas: &skia_safe::Canvas,
        BorrowedOpenGLTexture { texture_id, size, origin, .. }: &BorrowedOpenGLTexture,
    ) -> Option<skia_safe::Image> {
        unsafe {
            let mut texture_info = skia_safe::gpu::gl::TextureInfo::from_target_and_id(
                glow::TEXTURE_2D,
                texture_id.get(),
            );
            texture_info.format = glow::RGBA8;
            let backend_texture = skia_safe::gpu::backend_textures::make_gl(
                (size.width as _, size.height as _),
                skia_safe::gpu::Mipmapped::No,
                texture_info,
                "Borrowed GL texture",
            );
            skia_safe::image::Image::from_texture(
                canvas.recording_context().as_mut().unwrap(),
                &backend_texture,
                match origin {
                    i_slint_core::graphics::BorrowedOpenGLTextureOrigin::TopLeft => {
                        skia_safe::gpu::SurfaceOrigin::TopLeft
                    }
                    i_slint_core::graphics::BorrowedOpenGLTextureOrigin::BottomLeft => {
                        skia_safe::gpu::SurfaceOrigin::BottomLeft
                    }
                    _ => unimplemented!(
                        "internal error: missing implementation for BorrowedOpenGLTextureOrigin"
                    ),
                },
                skia_safe::ColorType::RGBA8888,
                skia_safe::AlphaType::Unpremul,
                crate::sampled_texture_color_space(crate::TextureEncoding::Unorm),
            )
        }
    }
}

impl OpenGLSurface {
    pub fn new_with_config(
        window_handle: Arc<dyn raw_window_handle::HasWindowHandle>,
        display_handle: Arc<dyn raw_window_handle::HasDisplayHandle>,
        size: PhysicalWindowSize,
        requested_opengl_version: Option<RequestedOpenGLVersion>,
        config_builder: glutin::config::ConfigTemplateBuilder,
        config_filter: Option<&dyn Fn(&glutin::config::Config) -> bool>,
    ) -> Result<Self, PlatformError> {
        let width: std::num::NonZeroU32 = size.width.try_into().map_err(|_| {
            format!("Attempting to create window surface with an invalid width: {}", size.width)
        })?;
        let height: std::num::NonZeroU32 = size.height.try_into().map_err(|_| {
            format!("Attempting to create window surface with an invalid height: {}", size.height)
        })?;

        let window_handle = window_handle
            .window_handle()
            .map_err(|e| format!("error obtaining window handle for skia opengl renderer: {e}"))?;
        let display_handle = display_handle
            .display_handle()
            .map_err(|e| format!("error obtaining display handle for skia opengl renderer: {e}"))?;

        let (current_glutin_context, glutin_surface) = Self::init_glutin(
            window_handle,
            display_handle,
            width,
            height,
            requested_opengl_version,
            config_builder,
            config_filter,
        )?;

        glutin_surface.resize(&current_glutin_context, width, height);

        let fb_info = {
            use glow::HasContext;

            let gl = unsafe {
                glow::Context::from_loader_function_cstr(|name| {
                    current_glutin_context.display().get_proc_address(name) as *const _
                })
            };
            let fboid = unsafe { gl.get_parameter_i32(glow::FRAMEBUFFER_BINDING) };

            skia_safe::gpu::gl::FramebufferInfo {
                fboid: fboid.try_into().map_err(|_| {
                    "Skia Renderer: Internal error, framebuffer binding returned signed id"
                        .to_string()
                })?,
                format: skia_safe::gpu::gl::Format::RGBA8.into(),
                ..Default::default()
            }
        };

        let gl_interface = skia_safe::gpu::gl::Interface::new_load_with_cstr(|name| {
            current_glutin_context.display().get_proc_address(name) as *const _
        })
        .ok_or_else(|| {
            "Skia Renderer: Internal Error: Could not create OpenGL Interface".to_string()
        })?;

        let mut gr_context =
            skia_safe::gpu::direct_contexts::make_gl(gl_interface, None).ok_or_else(|| {
                "Skia Renderer: Internal Error: Could not create Skia Direct Context from GL interface".to_string()
            })?;

        let width: i32 = size.width.try_into().map_err(|e| {
                format!("Attempting to create window surface with width that doesn't fit into non-zero i32: {e}")
            })?;
        let height: i32 = size.height.try_into().map_err(|e| {
                format!(
                    "Attempting to create window surface with height that doesn't fit into non-zero i32: {e}"
                )
            })?;

        let surface = Self::create_internal_surface(
            fb_info,
            &current_glutin_context,
            &mut gr_context,
            width,
            height,
        )?
        .into();

        Ok(Self {
            #[cfg(all(target_os = "android", feature = "experimental-swappy"))]
            swappy: crate::android_swappy::Swappy::new(
                window_handle.as_raw(), &current_glutin_context.display(), &glutin_surface,
            ),
            #[cfg(target_os = "android")]
            frame_timestamps: RefCell::default(),
            #[cfg(target_os = "android")]
            gpu_timestamps: crate::android_gpu_timestamps::Timestamps::from_property().map(RefCell::new),
            fb_info,
            surface,
            gr_context: RefCell::new(gr_context),
            glutin_context: GlutinContext::new(current_glutin_context),
            glutin_surface,
        })
    }

    fn init_glutin(
        _window_handle: raw_window_handle::WindowHandle<'_>,
        _display_handle: raw_window_handle::DisplayHandle<'_>,
        width: NonZeroU32,
        height: NonZeroU32,
        requested_opengl_version: Option<RequestedOpenGLVersion>,
        config_template_builder: glutin::config::ConfigTemplateBuilder,
        config_filter: Option<&dyn Fn(&glutin::config::Config) -> bool>,
    ) -> Result<
        (
            glutin::context::PossiblyCurrentContext,
            glutin::surface::Surface<glutin::surface::WindowSurface>,
        ),
        PlatformError,
    > {
        cfg_if::cfg_if! {
            if #[cfg(target_os = "macos")] {
                let display_api_preference = glutin::display::DisplayApiPreference::Cgl;
            } else if #[cfg(not(target_family = "windows"))] {
                let display_api_preference = glutin::display::DisplayApiPreference::Egl;
            } else {
                let display_api_preference = glutin::display::DisplayApiPreference::EglThenWgl(Some(_window_handle.as_raw()));
            }
        }

        let gl_display = unsafe {
            glutin::display::Display::new(_display_handle.as_raw(), display_api_preference)
                .map_err(|glutin_error| {
                    format!(
                        "Error creating glutin display for native display {:?}: {}",
                        _display_handle.as_raw(),
                        glutin_error
                    )
                })?
        };

        // On macOS, there's only one GL config and that's initialized based on the values in the config template
        // builder. So if that one has transparency enabled, it'll show up in the config, and will be set on the
        // context later. So we must enable it here, there's no way of enabling it later.
        // On EGL/GLX/WGL there are system provided configs that may or may not support transparency. Here in case
        // the system doesn't support transparency, we want to fall back to a config that doesn't - better than not
        // rendering anything at all. So we don't want to limit the configurations we get to see early on.
        // Commented out due to https://github.com/rust-windowing/glutin/issues/1640
        #[cfg(target_os = "macos")]
        let config_template_builder = config_template_builder.with_transparency(true);

        // Upstream advises to use this only on Windows.
        #[cfg(target_family = "windows")]
        let config_template_builder =
            config_template_builder.compatible_with_native_window(_window_handle.as_raw());

        let config_template = config_template_builder.build();

        let configs: Vec<glutin::config::Config> = unsafe {
            gl_display
                .find_configs(config_template)
                .map_err(|e| format!("Could not find valid OpenGL display configurations: {e}"))?
        }
        .filter(|config| config_filter.as_ref().is_none_or(|filter_fn| filter_fn(config)))
        .collect();

        // Front-buffer (single-buffer) experiment: when requested at launch, prefer
        // a config advertising EGL_MUTABLE_RENDER_BUFFER_BIT_KHR so the surface can
        // be switched to EGL_SINGLE_BUFFER at runtime (glutin's ConfigTemplateBuilder
        // cannot request it). Falls back to the normal set if the device exposes no
        // such config; default builds (prop unset) are unaffected.
        #[cfg(target_os = "android")]
        let configs: Vec<glutin::config::Config> =
            if front_buffer_requested() && configs.iter().any(|c| egl_config_is_mutable(c)) {
                configs.into_iter().filter(|c| egl_config_is_mutable(c)).collect()
            } else {
                configs
            };

        let config = configs
            .into_iter()
            .reduce(|accum, config| {
                let transparency_check = config.supports_transparency().unwrap_or(false)
                    & !accum.supports_transparency().unwrap_or(false);

                if transparency_check || config.num_samples() < accum.num_samples() {
                    config
                } else {
                    accum
                }
            })
            .ok_or("Unable to find suitable GL config")?;

        let requested_opengl_version =
            requested_opengl_version.unwrap_or(RequestedOpenGLVersion::OpenGLES(Some((3, 0))));
        let preferred_context_attributes = match requested_opengl_version {
            RequestedOpenGLVersion::OpenGL(version) => {
                let version =
                    version.map(|(major, minor)| glutin::context::Version { major, minor });
                ContextAttributesBuilder::new()
                    .with_context_api(ContextApi::OpenGl(version))
                    .build(Some(_window_handle.as_raw()))
            }
            RequestedOpenGLVersion::OpenGLES(version) => {
                let version =
                    version.map(|(major, minor)| glutin::context::Version { major, minor });

                ContextAttributesBuilder::new()
                    .with_context_api(ContextApi::Gles(version))
                    .build(Some(_window_handle.as_raw()))
            }
        };

        let gles2_fallback_context_attributes = ContextAttributesBuilder::new()
            .with_context_api(ContextApi::Gles(Some(glutin::context::Version {
                major: 2,
                minor: 0,
            })))
            .build(Some(_window_handle.as_raw()));

        let fallback_context_attributes =
            ContextAttributesBuilder::new().build(Some(_window_handle.as_raw()));

        let not_current_gl_context = unsafe {
            gl_display
                .create_context(&config, &preferred_context_attributes)
                .or_else(|_| gl_display.create_context(&config, &gles2_fallback_context_attributes))
                .or_else(|_| gl_display.create_context(&config, &fallback_context_attributes))
                .map_err(|e| format!("Error creating OpenGL context: {e}"))
        }?;

        let attrs = SurfaceAttributesBuilder::<WindowSurface>::new().build(
            _window_handle.as_raw(),
            width,
            height,
        );

        let surface = unsafe {
            config
                .display()
                .create_window_surface(&config, &attrs)
                .map_err(|e| format!("Error creating OpenGL window surface: {e}"))?
        };

        let context = not_current_gl_context.make_current(&surface)
            .map_err(|glutin_error: glutin::error::Error| -> PlatformError {
                format!("Skia Renderer: Failed to make newly created OpenGL context current: {glutin_error}")
                .into()
        })?;

        // Align the GL layer to the top-left, so that resizing only invalidates the bottom/right
        // part of the window.
        #[cfg(target_os = "macos")]
        if let raw_window_handle::RawWindowHandle::AppKit(raw_window_handle::AppKitWindowHandle {
            ns_view,
            ..
        }) = _window_handle.as_raw()
        {
            let ns_view: &objc2_app_kit::NSView = unsafe { ns_view.cast().as_ref() };
            ns_view.setLayerContentsPlacement(objc2_app_kit::NSViewLayerContentsPlacement::TopLeft);
        }

        // Sanity check, as all this might succeed on Windows without working GL drivers, but this will fail:
        if context
            .display()
            .get_proc_address(&std::ffi::CString::new("glCreateShader").unwrap())
            .is_null()
        {
            return Err(
                "Failed to initialize OpenGL driver: Could not locate glCreateShader symbol"
                    .to_string()
                    .into(),
            );
        }

        // Try to default to vsync and ignore if the driver doesn't support it.
        surface
            .set_swap_interval(
                &context,
                glutin::surface::SwapInterval::Wait(NonZeroU32::new(1).unwrap()),
            )
            .ok();

        Ok((context, surface))
    }

    fn create_internal_surface(
        fb_info: skia_safe::gpu::gl::FramebufferInfo,
        gl_context: &glutin::context::PossiblyCurrentContext,
        gr_context: &mut skia_safe::gpu::DirectContext,
        width: i32,
        height: i32,
    ) -> Result<skia_safe::Surface, PlatformError> {
        let config = gl_context.config();

        let backend_render_target = skia_safe::gpu::backend_render_targets::make_gl(
            (width, height),
            Some(config.num_samples() as _),
            config.stencil_size() as _,
            fb_info,
        );
        match skia_safe::gpu::surfaces::wrap_backend_render_target(
            gr_context,
            &backend_render_target,
            skia_safe::gpu::SurfaceOrigin::BottomLeft,
            skia_safe::ColorType::RGBA8888,
            crate::attachment_color_space(crate::TextureEncoding::Unorm),
            None,
        ) {
            Some(surface) => Ok(surface),
            None => {
                Err("Skia OpenGL Renderer: Failed to allocate internal backend rendering target"
                    .into())
            }
        }
    }

    fn ensure_context_current(&self) -> Result<(), PlatformError> {
        #[cfg(target_os = "android")]
        let _trace = crate::renderer_trace::Section::new(c"Chatter Skia EGL context check");
        if !self.glutin_context.is_current() {
            #[cfg(target_os = "android")]
            let _current_trace =
                crate::renderer_trace::Section::new(c"Chatter Skia EGL make current");
            self.glutin_context.make_current(&self.glutin_surface).map_err(
                |glutin_error| -> PlatformError {
                    format!("Skia Renderer: Error making context current: {glutin_error}").into()
                },
            )?;
        }
        Ok(())
    }
}

impl Drop for OpenGLSurface {
    fn drop(&mut self) {
        #[cfg(feature = "painted-scroll-probe")]
        i_slint_core::painted_scroll_probe::cancel();
        #[cfg(all(target_os = "android", feature = "experimental-swappy"))]
        drop(self.swappy.take());
        #[cfg(target_os = "android")]
        self.frame_timestamps
            .get_mut()
            .disable(&self.glutin_context.display(), &self.glutin_surface);
        // Make sure that the context is current before Skia calls glDelete***
        // In the event that this fails for some reason (lost GL context), convey that to Skia so that it doesn't try to call
        // glDelete***
        let context_current = self.ensure_context_current().is_ok();
        #[cfg(target_os = "android")]
        if let Some(timestamps) = &mut self.gpu_timestamps {
            timestamps.get_mut().teardown(context_current);
        }
        if !context_current {
            i_slint_core::debug_log!(
                "Skia OpenGL Renderer warning: Failed to make context current for destruction - considering context abandoned."
            );
            self.gr_context.borrow_mut().abandon();
        }
    }
}
