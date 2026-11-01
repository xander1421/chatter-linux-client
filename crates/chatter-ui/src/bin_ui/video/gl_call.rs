//! Call-video GL texture uploader (Linux).
//!
//! Eliminates the per-frame 30 MB Skia copy + full GPU re-upload that the
//! old `clone_from_slice` + `slint::Image::from_rgba8` path incurred for
//! remote call video (screen-share). Instead we keep ONE persistent GL
//! texture per participant and update it in place with `glTexSubImage2D`,
//! handing Slint a `BorrowedOpenGLTexture` image that borrows that texture
//! id. Slint's `from_rgba8` path produces an `ImageCacheKey::Invalid`
//! image, which forces Skia to `Data::new_copy` the whole RGBA buffer and
//! re-upload a fresh GPU texture on EVERY present — for a 5120×1440
//! screen-share that is ~30 MB copied + uploaded per frame.
//!
//! # Threading / GL context
//!
//! All GL work in this module MUST run on the render thread with the GL
//! context current. That is exactly when Slint's rendering notifier fires
//! ([`on_rendering_setup`] / [`on_before_rendering`] / [`on_teardown`]),
//! so every GL call below is reachable only from those entry points, which
//! `gl_linux.rs` invokes from inside the *single* rendering notifier it
//! owns (the notifier is single-slot and already taken by the GStreamer
//! media player — we multiplex it, we do not register a second one).
//!
//! The tokio-side frame subscriber (`timers.rs`) never touches GL: it only
//! publishes the latest `Arc<Vec<u8>>` RGBA + dimensions into
//! [`publish_batch`] (a cheap Arc clone, no 30 MB copy). [`on_before_rendering`]
//! drains that inbox on the GL thread and does the in-place upload.
//!
//! # Fallback
//!
//! If the graphics API is not `NativeOpenGL`, or the GL function table fails
//! to load, uploads are skipped. `timers.rs` still publishes into the inbox
//! and nothing is uploaded. We never panic in the notifier (that would take
//! down rendering).

use slint::ComponentHandle;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_void};
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::{Arc, LazyLock, Mutex};

use slint::{Model, ModelRc, VecModel};

// ---------------------------------------------------------------------------
// GL constants (GLES2/GLES3 / desktop GL share these values).
// ---------------------------------------------------------------------------

const GL_NO_ERROR: u32 = 0;
const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_RGBA: u32 = 0x1908;
const GL_UNSIGNED_BYTE: u32 = 0x1401;
const GL_TEXTURE_BINDING_2D: u32 = 0x8069;
const GL_UNPACK_ALIGNMENT: u32 = 0x0CF5;
const GL_UNPACK_ROW_LENGTH: u32 = 0x0CF2; // GLES3 / desktop GL only.
const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
const GL_TEXTURE_MAG_FILTER: u32 = 0x2800;
const GL_TEXTURE_WRAP_S: u32 = 0x2802;
const GL_TEXTURE_WRAP_T: u32 = 0x2803;
const GL_LINEAR: i32 = 0x2601;
const GL_CLAMP_TO_EDGE: i32 = 0x812F;

/// Drop a participant's texture if it has been absent from the inbox for
/// this many consecutive `on_before_rendering` passes. Avoids churning the
/// texture on a single dropped/late frame.
const MISS_FRAMES_BEFORE_DROP: u32 = 120;

// ---------------------------------------------------------------------------
// Hand-loaded GL function table.
//
// There is no `gl`/`glow` direct dependency to `use`, so we declare the FFI
// signatures by hand and resolve them through Slint's `get_proc_address`
// (same loader pattern `gl_linux.rs` uses for EGL).
// ---------------------------------------------------------------------------

#[allow(non_snake_case)]
struct GlFns {
    GenTextures: unsafe extern "C" fn(n: i32, textures: *mut u32),
    DeleteTextures: unsafe extern "C" fn(n: i32, textures: *const u32),
    BindTexture: unsafe extern "C" fn(target: u32, texture: u32),
    TexImage2D: unsafe extern "C" fn(
        target: u32,
        level: i32,
        internalformat: i32,
        width: i32,
        height: i32,
        border: i32,
        format: u32,
        ty: u32,
        pixels: *const c_void,
    ),
    TexSubImage2D: unsafe extern "C" fn(
        target: u32,
        level: i32,
        xoffset: i32,
        yoffset: i32,
        width: i32,
        height: i32,
        format: u32,
        ty: u32,
        pixels: *const c_void,
    ),
    TexParameteri: unsafe extern "C" fn(target: u32, pname: u32, param: i32),
    PixelStorei: unsafe extern "C" fn(pname: u32, param: i32),
    GetIntegerv: unsafe extern "C" fn(pname: u32, params: *mut i32),
    GetError: unsafe extern "C" fn() -> u32,
}

impl GlFns {
    /// Resolve every required symbol via `get_proc_address`. Returns `None`
    /// (graceful degrade) if any symbol is missing.
    fn load(get_proc_address: &dyn Fn(&CStr) -> *const c_void) -> Option<GlFns> {
        // Helper: resolve a symbol or bail.
        macro_rules! sym {
            ($name:literal) => {{
                let cname = CString::new($name).ok()?;
                let ptr = get_proc_address(cname.as_c_str());
                if ptr.is_null() {
                    tracing::warn!("gl_call: failed to resolve GL symbol '{}'", $name);
                    return None;
                }
                ptr
            }};
        }

        // SAFETY: each pointer resolves to the GL entry point of the matching
        // name; the transmuted signatures mirror the canonical GL prototypes.
        unsafe {
            Some(GlFns {
                GenTextures: std::mem::transmute::<*const c_void, _>(sym!("glGenTextures")),
                DeleteTextures: std::mem::transmute::<*const c_void, _>(sym!("glDeleteTextures")),
                BindTexture: std::mem::transmute::<*const c_void, _>(sym!("glBindTexture")),
                TexImage2D: std::mem::transmute::<*const c_void, _>(sym!("glTexImage2D")),
                TexSubImage2D: std::mem::transmute::<*const c_void, _>(sym!("glTexSubImage2D")),
                TexParameteri: std::mem::transmute::<*const c_void, _>(sym!("glTexParameteri")),
                PixelStorei: std::mem::transmute::<*const c_void, _>(sym!("glPixelStorei")),
                GetIntegerv: std::mem::transmute::<*const c_void, _>(sym!("glGetIntegerv")),
                GetError: std::mem::transmute::<*const c_void, _>(sym!("glGetError")),
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Per-participant persistent texture.
// ---------------------------------------------------------------------------

struct CallTexture {
    /// GL texture id (non-zero; created with `glGenTextures`).
    id: NonZeroU32,
    /// Current allocated width/height of the texture's storage. When the
    /// incoming frame matches this, we `glTexSubImage2D`; otherwise we
    /// re-allocate with `glTexImage2D`.
    width: u32,
    height: u32,
    /// Last generation we uploaded; lets us skip a re-upload when the inbox
    /// still holds a frame we already pushed this present.
    last_generation: u64,
    /// Consecutive `on_before_rendering` passes this participant was absent
    /// from the inbox. Reset to 0 when seen; texture dropped past
    /// `MISS_FRAMES_BEFORE_DROP`.
    miss_frames: u32,
}

// ---------------------------------------------------------------------------
// Thread-local GL state (lives on the render thread only).
// ---------------------------------------------------------------------------

struct CallGlState {
    gl: GlFns,
    /// participant_id -> persistent texture.
    textures: HashMap<String, CallTexture>,
    /// Stable model bound ONCE to `call_remote_video_frames`. We mutate rows
    /// in place (`set_row_data`/`push`/`remove`) instead of rebuilding it.
    model: Option<Rc<VecModel<slint::Image>>>,
    /// Persistent texture for the media-file player (gl_linux.rs). Separate
    /// from the per-participant call textures so it is never subject to the
    /// miss-frame drop logic. Uploaded via [`upload_media_player_frame`].
    player_tex: Option<CallTexture>,
    remote_keys: Vec<Option<(String, u64, u32, u32)>>,
    local_key: Option<(String, u64, u32, u32)>,
}

thread_local! {
    static CALL_GL: RefCell<Option<CallGlState>> = const { RefCell::new(None) };
}

// ---------------------------------------------------------------------------
// Cross-thread frame inbox.
//
// Written by the tokio frame subscriber (`timers.rs::publish` -> here), read
// by the GL thread in `on_before_rendering`. `Arc<Vec<u8>>` is Send+Sync, so
// the publish is a cheap refcount bump — NO 30 MB copy.
// ---------------------------------------------------------------------------

/// One published frame: (rgba, width, height, rotation_degrees, generation).
type InboxEntry = (Arc<Vec<u8>>, u32, u32, u32, u64);

// `HashMap::new` is not const, so the mutex cannot be a plain static.
static REDRAW_PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub fn clear_redraw_pending() {
    REDRAW_PENDING.store(false, std::sync::atomic::Ordering::Release);
}

static CALL_FRAME_INBOX: LazyLock<Mutex<HashMap<String, InboxEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn inbox() -> &'static Mutex<HashMap<String, InboxEntry>> {
    &CALL_FRAME_INBOX
}

/// Replace the whole inbox with the given stable-ordered batch.
///
/// `timers.rs` calls this once per subscriber callback with the full current
/// participant set so that participants who vanished from the snapshot also
/// vanish from the inbox (and eventually get their texture dropped). Entries
/// are `(participant_id, rgba, width, height, rotation, generation)`.
pub fn publish_batch(frames: Vec<(String, Arc<Vec<u8>>, u32, u32, u32, u64)>) -> bool {
    if let Ok(mut map) = inbox().lock() {
        map.clear();
        for (id, rgba, w, h, rot, generation) in frames {
            map.insert(id, (rgba, w, h, rot, generation));
        }
    }
    !REDRAW_PENDING.swap(true, std::sync::atomic::Ordering::AcqRel)
}

/// Clear the inbox (call teardown / call end).
pub fn clear_inbox() {
    clear_redraw_pending();
    if let Ok(mut map) = inbox().lock() {
        map.clear();
    }
}

// ---------------------------------------------------------------------------
// Notifier entry points (called from gl_linux.rs's rendering notifier).
// ---------------------------------------------------------------------------

/// `RenderingState::RenderingSetup`: load the GL function table once.
///
/// Safe to call repeatedly; only the first successful load takes effect.
/// On a non-OpenGL graphics API, or if symbol resolution fails, this is a
/// no-op and frames are not uploaded (graceful degrade).
pub fn on_rendering_setup(graphics_api: &slint::GraphicsAPI<'_>) {
    let already = CALL_GL.with(|cell| cell.borrow().is_some());
    if already {
        return;
    }

    let gl = match graphics_api {
        slint::GraphicsAPI::NativeOpenGL { get_proc_address } => {
            match GlFns::load(get_proc_address) {
                Some(gl) => gl,
                None => {
                    tracing::warn!(
                        "gl_call: GL function table load failed - call video GL upload disabled"
                    );
                    return;
                }
            }
        }
        _ => {
            tracing::info!(
                "gl_call: graphics API is not NativeOpenGL - call video GL upload disabled"
            );
            return;
        }
    };

    CALL_GL.with(|cell| {
        *cell.borrow_mut() = Some(CallGlState {
            gl,
            textures: HashMap::new(),
            model: None,
            player_tex: None,
            remote_keys: Vec::new(),
            local_key: None,
        });
    });
    tracing::info!("gl_call: GL texture uploader initialized for call video");
}

/// `RenderingState::BeforeRendering`: drain the inbox and upload textures.
///
/// Runs on the GL thread with the context current. Saves and restores all GL
/// state it touches (`TEXTURE_BINDING_2D`, `UNPACK_ALIGNMENT`,
/// `UNPACK_ROW_LENGTH`) so Skia's own rendering is not corrupted — the
/// notifier contract requires perfect state preservation.
///
/// `window` is the `MainWindow`; we set `call_remote_video_frames` on it with
/// the persistent model populated with `BorrowedOpenGLTexture` images.
pub fn on_before_rendering(window: &crate::MainWindow) {
    // Release before snapshotting: a concurrent publisher can schedule the
    // next present, while this one always consumes the newest queued pixels.
    clear_redraw_pending();
    CALL_GL.with(|cell| {
        let mut guard = cell.borrow_mut();
        let state = match guard.as_mut() {
            Some(s) => s,
            None => return, // not active; nothing to do
        };

        // Snapshot the inbox (cheap: Arc clones), drop the lock fast so the
        // tokio publisher isn't blocked during GPU upload.
        let snapshot: Vec<(String, InboxEntry)> = match inbox().lock() {
            Ok(map) => map.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            Err(_) => return,
        };

        // Destructure into disjoint field borrows so we can call GL functions
        // (`&gl`) while mutating the texture map (`&mut textures`) in the same
        // scope — a single `&state` borrow would forbid that.
        let CallGlState {
            gl,
            textures,
            model,
            remote_keys,
            local_key,
            ..
        } = state;

        // Save GL state we are about to clobber.
        let mut prev_tex_binding: i32 = 0;
        let mut prev_unpack_alignment: i32 = 4;
        let mut prev_unpack_row_length: i32 = 0;
        // SAFETY: GL context is current (we're in the rendering notifier).
        unsafe {
            (gl.GetIntegerv)(GL_TEXTURE_BINDING_2D, &mut prev_tex_binding);
            (gl.GetIntegerv)(GL_UNPACK_ALIGNMENT, &mut prev_unpack_alignment);
            // UNPACK_ROW_LENGTH only exists on GLES3/desktop GL. Query it but
            // swallow any error so plain-GLES2 contexts still work.
            let _ = (gl.GetError)(); // clear pending errors before probe
            (gl.GetIntegerv)(GL_UNPACK_ROW_LENGTH, &mut prev_unpack_row_length);
            let row_length_supported = (gl.GetError)() == GL_NO_ERROR;

            // Tightly-packed RGBA: alignment 1, no row stride.
            (gl.PixelStorei)(GL_UNPACK_ALIGNMENT, 1);
            if row_length_supported {
                (gl.PixelStorei)(GL_UNPACK_ROW_LENGTH, 0);
            }

            // Mark every existing texture as a miss; cleared below if seen.
            for tex in textures.values_mut() {
                tex.miss_frames = tex.miss_frames.saturating_add(1);
            }

            // Upload each participant's frame into its persistent texture.
            for (id, (rgba, w, h, _rotation, generation)) in &snapshot {
                let w = *w;
                let h = *h;
                let generation = *generation;
                if w == 0 || h == 0 {
                    continue;
                }
                let expected = (w as usize) * (h as usize) * 4;
                if rgba.len() < expected {
                    tracing::warn!(
                        "gl_call: frame for {} too small ({} < {}), skipping",
                        id,
                        rgba.len(),
                        expected
                    );
                    continue;
                }

                // Look up or create this participant's texture.
                let need_alloc;
                {
                    let entry = textures.get(id);
                    need_alloc = match entry {
                        Some(t) => t.width != w || t.height != h,
                        None => true,
                    };
                }

                if textures.get(id).is_none() {
                    // Create a fresh GL texture object.
                    let mut tex_id: u32 = 0;
                    (gl.GenTextures)(1, &mut tex_id);
                    let nz = match NonZeroU32::new(tex_id) {
                        Some(nz) => nz,
                        None => {
                            tracing::error!("gl_call: glGenTextures returned 0 for {}", id);
                            continue;
                        }
                    };
                    (gl.BindTexture)(GL_TEXTURE_2D, tex_id);
                    // GLES sampling needs explicit filter + wrap or sampling
                    // is undefined (mips absent).
                    (gl.TexParameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
                    (gl.TexParameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
                    (gl.TexParameteri)(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
                    (gl.TexParameteri)(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
                    textures.insert(
                        id.clone(),
                        CallTexture {
                            id: nz,
                            width: 0,
                            height: 0,
                            last_generation: 0,
                            miss_frames: 0,
                        },
                    );
                }

                let tex = textures.get_mut(id).expect("just inserted");
                tex.miss_frames = 0;

                // Skip re-upload if we already pushed this exact frame and the
                // texture is already the right size.
                if !need_alloc && tex.last_generation == generation {
                    continue;
                }

                (gl.BindTexture)(GL_TEXTURE_2D, tex.id.get());
                let pixels = rgba.as_ptr() as *const c_void;
                if need_alloc {
                    // (Re)allocate storage at the new size.
                    (gl.TexImage2D)(
                        GL_TEXTURE_2D,
                        0,
                        GL_RGBA as i32,
                        w as i32,
                        h as i32,
                        0,
                        GL_RGBA,
                        GL_UNSIGNED_BYTE,
                        pixels,
                    );
                    tex.width = w;
                    tex.height = h;
                } else {
                    // In-place update of existing storage (the fast path).
                    (gl.TexSubImage2D)(
                        GL_TEXTURE_2D,
                        0,
                        0,
                        0,
                        w as i32,
                        h as i32,
                        GL_RGBA,
                        GL_UNSIGNED_BYTE,
                        pixels,
                    );
                }
                tex.last_generation = generation;
            }

            // Restore GL state before Slint/Skia draws.
            (gl.BindTexture)(GL_TEXTURE_2D, prev_tex_binding.max(0) as u32);
            (gl.PixelStorei)(GL_UNPACK_ALIGNMENT, prev_unpack_alignment);
            if row_length_supported {
                (gl.PixelStorei)(GL_UNPACK_ROW_LENGTH, prev_unpack_row_length);
            }
            // Drop any GL errors we generated so we don't leak them into Skia.
            let _ = (gl.GetError)();
        }

        // Drop textures for participants gone for too long.
        let to_drop: Vec<String> = textures
            .iter()
            .filter(|(_, t)| t.miss_frames > MISS_FRAMES_BEFORE_DROP)
            .map(|(id, _)| id.clone())
            .collect();
        if !to_drop.is_empty() {
            // SAFETY: GL context current.
            unsafe {
                for id in &to_drop {
                    if let Some(tex) = textures.remove(id) {
                        let raw = tex.id.get();
                        (gl.DeleteTextures)(1, &raw);
                    }
                }
            }
        }

        // Build the image list to MIRROR the participant tiles: one slot per
        // remote tile, in the SAME order the tiles render, looked up by the
        // tile's user_id — blank where that participant has no texture yet.
        // Positional indexing then always maps remote-video-frames[i] to
        // participants[i], even when a peer's stream count changes (e.g. camera
        // off while still screen-sharing) or a tile is a video-off avatar.
        // The local participant is pinned last and draws local-video-frame, so
        // it is excluded here.
        let participants = window
            .global::<crate::CallView>()
            .get_active_call_data()
            .participants;
        let remote_count = participants.row_count().saturating_sub(1);
        let key_for = |index| {
            let pid = participants.row_data(index)?.user_id.to_string();
            let tex = textures.get(&pid)?;
            (tex.width != 0 && tex.height != 0).then_some((
                pid,
                tex.last_generation,
                tex.width,
                tex.height,
            ))
        };
        let new_keys: Vec<_> = (0..remote_count).map(key_for).collect();
        let new_local_key = participants.row_count().checked_sub(1).and_then(key_for);
        if model.is_some() && *remote_keys == new_keys && *local_key == new_local_key {
            return;
        }
        let image_for = |key: &Option<(String, u64, u32, u32)>| {
            key.as_ref()
                .and_then(|(pid, _, _, _)| textures.get(pid))
                .map(|tex| {
                    // SAFETY: the texture stays alive across this frame's draw.
                    unsafe {
                        slint::BorrowedOpenGLTextureBuilder::new_gl_2d_rgba_texture(
                            tex.id,
                            [tex.width, tex.height].into(),
                        )
                        .build()
                    }
                })
                .unwrap_or_default()
        };

        // Populate the persistent model in place and bind it once. `model` is
        // the destructured `&mut Option<Rc<VecModel<Image>>>` field.
        let vm = match model.as_ref() {
            Some(m) => m.clone(),
            None => {
                let m = Rc::new(VecModel::<slint::Image>::default());
                window
                    .global::<crate::CallView>()
                    .set_call_remote_video_frames(ModelRc::from(m.clone()));
                *model = Some(m.clone());
                m
            }
        };

        let new_len = new_keys.len();
        let old_len = vm.row_count();
        for (i, key) in new_keys.iter().enumerate() {
            if i < old_len {
                if remote_keys.get(i) != Some(key) {
                    vm.set_row_data(i, image_for(key));
                }
            } else {
                vm.push(image_for(key));
            }
        }
        for i in (new_len..old_len).rev() {
            vm.remove(i);
        }
        if *local_key != new_local_key {
            window
                .global::<crate::CallView>()
                .set_call_local_video_frame(image_for(&new_local_key));
        }
        *remote_keys = new_keys;
        *local_key = new_local_key;
    });
}

/// Upload a single RGBA frame for the media-file player into a persistent GL
/// texture and return a `BorrowedOpenGLTexture` Slint image referencing it.
///
/// This is the media player's GPU upload path (used by `gl_linux.rs` from its
/// `BeforeRendering` notifier, i.e. the render thread with the GL context
/// current). It deliberately mirrors [`on_before_rendering`] rather than using
/// GStreamer's `glupload`/`glcolorconvert`: those mishandle the padded,
/// software-decoded buffers some demuxers/decoders (e.g. AVI / `avdec_mpeg4`)
/// produce, rendering a solid green frame. Doing the `glTexImage2D` /
/// `glTexSubImage2D` ourselves, tightly packed, in Slint's own context is
/// reliable for every format.
///
/// `rgba` MUST be tightly packed (stride == width*4). Returns `None` if the GL
/// uploader is not active (non-OpenGL backend) or the frame is malformed —
/// callers should fall back accordingly.
pub fn upload_media_player_frame(rgba: &[u8], width: u32, height: u32) -> Option<slint::Image> {
    if width == 0 || height == 0 {
        return None;
    }
    let expected = (width as usize) * (height as usize) * 4;
    if rgba.len() < expected {
        tracing::warn!(
            "gl_call: media player frame too small ({} < {}), skipping",
            rgba.len(),
            expected
        );
        return None;
    }

    CALL_GL.with(|cell| {
        let mut guard = cell.borrow_mut();
        let state = guard.as_mut()?; // not active -> None (caller falls back)
        let CallGlState { gl, player_tex, .. } = state;

        // SAFETY: GL context is current (we're called from the rendering notifier).
        unsafe {
            // Save the GL state we clobber so Slint/Skia's own draw is intact.
            let mut prev_tex_binding: i32 = 0;
            let mut prev_unpack_alignment: i32 = 4;
            let mut prev_unpack_row_length: i32 = 0;
            (gl.GetIntegerv)(GL_TEXTURE_BINDING_2D, &mut prev_tex_binding);
            (gl.GetIntegerv)(GL_UNPACK_ALIGNMENT, &mut prev_unpack_alignment);
            let _ = (gl.GetError)();
            (gl.GetIntegerv)(GL_UNPACK_ROW_LENGTH, &mut prev_unpack_row_length);
            let row_length_supported = (gl.GetError)() == GL_NO_ERROR;

            (gl.PixelStorei)(GL_UNPACK_ALIGNMENT, 1);
            if row_length_supported {
                (gl.PixelStorei)(GL_UNPACK_ROW_LENGTH, 0);
            }

            // Create the texture object once.
            if player_tex.is_none() {
                let mut tex_id: u32 = 0;
                (gl.GenTextures)(1, &mut tex_id);
                let nz = NonZeroU32::new(tex_id)?;
                (gl.BindTexture)(GL_TEXTURE_2D, tex_id);
                (gl.TexParameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
                (gl.TexParameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
                (gl.TexParameteri)(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
                (gl.TexParameteri)(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
                *player_tex = Some(CallTexture {
                    id: nz,
                    width: 0,
                    height: 0,
                    last_generation: 0,
                    miss_frames: 0,
                });
            }

            let tex = player_tex.as_mut()?;
            (gl.BindTexture)(GL_TEXTURE_2D, tex.id.get());
            let pixels = rgba.as_ptr() as *const c_void;
            if tex.width != width || tex.height != height {
                (gl.TexImage2D)(
                    GL_TEXTURE_2D,
                    0,
                    GL_RGBA as i32,
                    width as i32,
                    height as i32,
                    0,
                    GL_RGBA,
                    GL_UNSIGNED_BYTE,
                    pixels,
                );
                tex.width = width;
                tex.height = height;
            } else {
                (gl.TexSubImage2D)(
                    GL_TEXTURE_2D,
                    0,
                    0,
                    0,
                    width as i32,
                    height as i32,
                    GL_RGBA,
                    GL_UNSIGNED_BYTE,
                    pixels,
                );
            }
            let tex_id = tex.id;

            // Restore clobbered GL state and swallow our errors.
            (gl.BindTexture)(GL_TEXTURE_2D, prev_tex_binding.max(0) as u32);
            (gl.PixelStorei)(GL_UNPACK_ALIGNMENT, prev_unpack_alignment);
            if row_length_supported {
                (gl.PixelStorei)(GL_UNPACK_ROW_LENGTH, prev_unpack_row_length);
            }
            let _ = (gl.GetError)();

            // SAFETY: tex_id is a live, non-zero GL 2D RGBA texture kept alive in
            // player_tex across presents; Slint reads it during its own draw this
            // same frame, and we never delete it mid-frame.
            Some(
                slint::BorrowedOpenGLTextureBuilder::new_gl_2d_rgba_texture(
                    tex_id,
                    [width, height].into(),
                )
                .build(),
            )
        }
    })
}

/// `RenderingState::RenderingTeardown`: delete all textures and clear state.
///
/// Runs on the GL thread with the context current (Slint guarantees this for
/// the teardown callback).
pub fn on_teardown() {
    CALL_GL.with(|cell| {
        if let Some(state) = cell.borrow_mut().take() {
            // SAFETY: GL context current during teardown.
            unsafe {
                for (_, tex) in state.textures.iter() {
                    let raw = tex.id.get();
                    (state.gl.DeleteTextures)(1, &raw);
                }
                if let Some(tex) = state.player_tex {
                    let raw = tex.id.get();
                    (state.gl.DeleteTextures)(1, &raw);
                }
            }
        }
    });
    clear_inbox();
}
