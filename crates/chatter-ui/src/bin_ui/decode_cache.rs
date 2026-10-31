//! Decoded-image cache: sender avatars, attachment thumbnails, inline GIF
//! frames.
//!
//! Opening a conversation used to decode every image it showed from scratch,
//! every time — the same sender avatar once per row, the same thumbnail on
//! every reopen, every GIF decrypted and decoded again. This keeps the decoded
//! pixels so a reopen is a lookup.
//!
//! **Memory only, never disk.** Nothing here is written anywhere: the source
//! bytes stay in SQLCipher / the encrypted attachment store as before, and the
//! decoded copies live in this process until evicted or [`wipe`]d. They are
//! the same pixels the open conversation's model already holds — buffers are
//! reference-counted, so a cached image on screen costs no second copy.
//!
//! Wiped on logout ([`on_logout`]) and when the app is hidden on Android
//! ([`on_lock`] — Activity pause: screen locked or app backgrounded). Desktop
//! has no lock state; there the cache lives until logout or exit. Wiping
//! drops the buffers; freed memory is not zeroed, the same as every other
//! decoded image the UI has ever shown.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, LazyLock, Mutex};

use slint::{Rgba8Pixel, SharedPixelBuffer};

pub type Pixels = SharedPixelBuffer<Rgba8Pixel>;

/// Long edge of a cached row avatar. The row draws it at 32 logical px
/// (`Spacing.avatar-sm`); 128 covers a 4x display. The stored avatar is 512 px
/// lossless WebP, ~4.5 ms to decode on desktop and ~1 MB of RGBA per copy.
pub const AVATAR_PX: u32 = 128;

const AVATAR_BUDGET: usize = 6 << 20;
const THUMBNAIL_BUDGET: usize = 32 << 20;
const GIF_BUDGET: usize = 24 << 20;

/// Every frame of one inline GIF, with its delays in ms.
#[derive(Clone)]
pub struct GifFrames {
    pub frames: Arc<Vec<Pixels>>,
    pub delays: Arc<Vec<u64>>,
}

/// Least-recently-used map bounded by the bytes its values hold.
pub struct ByteLru<K, V> {
    entries: HashMap<K, (V, usize, u64)>,
    budget: usize,
    used: usize,
    tick: u64,
}

impl<K: Eq + Hash + Clone, V: Clone> ByteLru<K, V> {
    pub fn new(budget: usize) -> Self {
        Self {
            entries: HashMap::new(),
            budget,
            used: 0,
            tick: 0,
        }
    }

    pub fn get(&mut self, key: &K) -> Option<V> {
        self.tick += 1;
        let tick = self.tick;
        self.entries.get_mut(key).map(|entry| {
            entry.2 = tick;
            entry.0.clone()
        })
    }

    /// Insert, evicting the least recently used entries to stay within the
    /// budget. A value larger than the whole budget is not kept.
    pub fn insert(&mut self, key: K, value: V, bytes: usize) {
        if let Some((_, old, _)) = self.entries.remove(&key) {
            self.used -= old;
        }
        if bytes > self.budget {
            return;
        }
        while self.used + bytes > self.budget {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (_, _, tick))| *tick)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some((_, size, _)) = self.entries.remove(&oldest) {
                self.used -= size;
            }
        }
        self.tick += 1;
        self.used += bytes;
        self.entries.insert(key, (value, bytes, self.tick));
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.used = 0;
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&K) -> bool) {
        self.entries.retain(|key, (_, bytes, _)| {
            if keep(key) {
                true
            } else {
                self.used -= *bytes;
                false
            }
        });
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn used(&self) -> usize {
        self.used
    }
}

struct Caches {
    generation: u64,
    /// Keyed by a hash of the avatar's encoded bytes, so a changed avatar is
    /// a new entry rather than a stale hit.
    avatars: ByteLru<u64, Pixels>,
    /// Keyed by attachment id and the stored thumbnail's length: the bytes of
    /// an attachment's thumbnail do not change once present, and the length
    /// tells a later-arriving thumbnail from the local-file fallback (len 0).
    thumbnails: ByteLru<(String, usize), Pixels>,
    gifs: ByteLru<String, GifFrames>,
}

impl Caches {
    fn new() -> Self {
        Self {
            generation: 0,
            avatars: ByteLru::new(AVATAR_BUDGET),
            thumbnails: ByteLru::new(THUMBNAIL_BUDGET),
            gifs: ByteLru::new(GIF_BUDGET),
        }
    }

    fn wipe(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.avatars.clear();
        self.thumbnails.clear();
        self.gifs.clear();
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.avatars.len() == 0 && self.thumbnails.len() == 0 && self.gifs.len() == 0
    }
}

static CACHES: LazyLock<Mutex<Caches>> = LazyLock::new(|| Mutex::new(Caches::new()));

fn caches() -> std::sync::MutexGuard<'static, Caches> {
    CACHES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn pixel_bytes(p: &Pixels) -> usize {
    p.width() as usize * p.height() as usize * 4
}

fn to_pixels(img: image::DynamicImage) -> Pixels {
    let rgba = img.into_rgba8();
    let (w, h) = rgba.dimensions();
    Pixels::clone_from_slice(rgba.as_raw(), w, h)
}

/// A sender avatar decoded at [`AVATAR_PX`], shared by every row it appears
/// on. `None` when the bytes do not decode.
pub fn avatar(bytes: &[u8]) -> Option<Pixels> {
    avatar_guarded(bytes, thumbnail_generation())
}

pub fn avatar_guarded(bytes: &[u8], generation: u64) -> Option<Pixels> {
    if generation != thumbnail_generation() {
        return None;
    }
    if bytes.is_empty() {
        return None;
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    let key = hasher.finish();
    {
        let mut cache = caches();
        if cache.generation != generation {
            return None;
        }
        if let Some(hit) = cache.avatars.get(&key) {
            return Some(hit);
        }
    }
    // Decoded outside the lock: two threads racing on one avatar both decode,
    // which is the old cost, not a correctness problem.
    let pixels = chatter_core::media::ImageProcessor::inline_pixels_from_memory(bytes)
        .ok()
        .map(image::DynamicImage::ImageRgba8)
        .map(|img| to_pixels(img.thumbnail(AVATAR_PX, AVATAR_PX)))?;
    let mut cache = caches();
    if cache.generation != generation {
        return None;
    }
    cache
        .avatars
        .insert(key, pixels.clone(), pixel_bytes(&pixels));
    Some(pixels)
}

/// The decoded thumbnail of attachment `id`, from `decode` on a miss.
/// `source_len` is the length of the bytes `decode` reads (0 for the
/// local-file fallback).
pub fn thumbnail(
    id: &str,
    source_len: usize,
    decode: impl FnOnce() -> Option<image::DynamicImage>,
) -> Option<Pixels> {
    thumbnail_guarded(id, source_len, thumbnail_generation(), decode)
}

pub fn thumbnail_guarded(
    id: &str,
    source_len: usize,
    generation: u64,
    decode: impl FnOnce() -> Option<image::DynamicImage>,
) -> Option<Pixels> {
    let key = (id.to_string(), source_len);
    {
        let mut cache = caches();
        if cache.generation != generation {
            return None;
        }
        if let Some(hit) = cache.thumbnails.get(&key) {
            return Some(hit);
        }
    }
    let pixels = decode().map(to_pixels)?;
    let mut cache = caches();
    // Logout/Android pause may wipe while the decoder is running. Never
    // repopulate that cleared cache or publish old pixels after the wipe.
    if cache.generation != generation {
        return None;
    }
    cache
        .thumbnails
        .insert(key, pixels.clone(), pixel_bytes(&pixels));
    Some(pixels)
}

/// Capture before asynchronous core preparation; lock/logout revokes the token.
pub fn thumbnail_generation() -> u64 {
    caches().generation
}

/// Cache already prepared pixels only if the initiating cache lifetime remains.
pub fn prepared_thumbnail(id: &str, generation: u64, image: image::DynamicImage) -> Option<Pixels> {
    let pixels = to_pixels(image);
    let mut cache = caches();
    if cache.generation != generation {
        return None;
    }
    cache
        .thumbnails
        .insert((id.to_string(), 0), pixels.clone(), pixel_bytes(&pixels));
    Some(pixels)
}

/// Read already prepared tray pixels without any file IO or decoding.
pub fn cached_thumbnail(id: &str, source_len: usize) -> Option<Pixels> {
    caches().thumbnails.get(&(id.to_string(), source_len))
}

pub fn gif(id: &str) -> Option<GifFrames> {
    caches().gifs.get(&id.to_string())
}

pub fn put_gif(id: &str, frames: Vec<Pixels>, delays: Vec<u64>) {
    put_gif_guarded(id, thumbnail_generation(), frames, delays);
}

pub fn put_gif_guarded(id: &str, generation: u64, frames: Vec<Pixels>, delays: Vec<u64>) {
    let bytes = frames.iter().map(pixel_bytes).sum();
    let mut cache = caches();
    if cache.generation != generation {
        return;
    }
    cache.gifs.insert(
        id.to_string(),
        GifFrames {
            frames: Arc::new(frames),
            delays: Arc::new(delays),
        },
        bytes,
    );
}

/// Drop decoded media whose attachment rows were removed by expiry.
pub fn evict_attachments(ids: &std::collections::HashSet<String>) {
    let mut cache = caches();
    cache.thumbnails.retain(|(id, _)| !ids.contains(id));
    cache.gifs.retain(|id| !ids.contains(id));
}

/// Drop every decoded image.
pub fn wipe() {
    chatter_core::media::static_preview::wipe();
    caches().wipe();
}

/// The app was hidden (Android Activity pause: screen locked, backgrounded).
pub fn on_lock() {
    wipe();
}

/// The account signed out.
pub fn on_logout() {
    wipe();
}

#[cfg(test)]
pub(crate) fn is_empty() -> bool {
    caches().is_empty()
}

/// Serialises tests that touch the process-wide cache.
#[cfg(test)]
pub(crate) static TEST_SERIAL: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    fn webp(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_fn(w, h, |x, y| image::Rgba([x as u8, y as u8, 7, 255]));
        let mut out = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut out)
            .encode(img.as_raw(), w, h, image::ExtendedColorType::Rgba8)
            .unwrap();
        out
    }

    fn fill() {
        assert!(avatar(&webp(512, 512)).is_some());
        assert!(thumbnail("att-1", 10, || Some(image::DynamicImage::new_rgba8(8, 8))).is_some());
        put_gif(
            "gif-1",
            vec![Pixels::new(4, 4), Pixels::new(4, 4)],
            vec![100, 100],
        );
        assert!(!is_empty());
    }

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner())
    }

    #[test]
    fn lock_wipes_every_cache() {
        let _g = guard();
        fill();
        on_lock();
        assert!(is_empty());
        assert!(gif("gif-1").is_none());
    }

    #[test]
    fn wipe_during_thumbnail_decode_cannot_repopulate_cache() {
        let _g = guard();
        wipe();
        assert!(
            thumbnail("stale", 0, || {
                on_lock();
                Some(image::DynamicImage::new_rgba8(8, 8))
            })
            .is_none()
        );
        assert!(cached_thumbnail("stale", 0).is_none());
    }

    #[test]
    fn one_wipe_revokes_every_subsequent_decode_in_the_initiating_batch() {
        let _g = guard();
        wipe();
        let generation = thumbnail_generation();
        assert!(
            thumbnail_guarded("first", 10, generation, || {
                on_lock();
                Some(image::DynamicImage::new_rgba8(8, 8))
            })
            .is_none()
        );
        assert!(
            thumbnail_guarded("second", 10, generation, || panic!(
                "revoked decoder started"
            ))
            .is_none()
        );
        assert!(avatar_guarded(&webp(8, 8), generation).is_none());
        assert!(is_empty());
    }

    #[test]
    fn asynchronous_preparation_cannot_cache_after_its_generation_was_wiped() {
        let _g = guard();
        let generation = thumbnail_generation();
        on_lock();
        assert!(
            prepared_thumbnail(
                "stale-async",
                generation,
                image::DynamicImage::new_rgba8(8, 8)
            )
            .is_none()
        );
        assert!(cached_thumbnail("stale-async", 0).is_none());
    }

    #[test]
    fn delayed_gif_preparation_cannot_repopulate_after_logout() {
        let _g = guard();
        wipe();
        let generation = thumbnail_generation();
        on_logout();
        put_gif_guarded(
            "stale-gif",
            generation,
            vec![Pixels::new(4, 4), Pixels::new(4, 4)],
            vec![40, 60],
        );
        assert!(gif("stale-gif").is_none());
        put_gif_guarded(
            "current-gif",
            thumbnail_generation(),
            vec![Pixels::new(4, 4), Pixels::new(4, 4)],
            vec![40, 60],
        );
        assert_eq!(gif("current-gif").unwrap().delays.as_slice(), &[40, 60]);
        wipe();
    }

    #[test]
    fn logout_wipes_every_cache() {
        let _g = guard();
        fill();
        on_logout();
        assert!(is_empty());
        assert!(gif("gif-1").is_none());
    }

    #[test]
    fn avatar_is_decoded_once_at_row_size_and_shared() {
        let _g = guard();
        wipe();
        let bytes = webp(512, 512);
        let a = avatar(&bytes).unwrap();
        let b = avatar(&bytes).unwrap();
        assert_eq!((a.width(), a.height()), (AVATAR_PX, AVATAR_PX));
        // Same buffer, not a second decode: SharedPixelBuffer clones share storage.
        assert_eq!(a.as_bytes().as_ptr(), b.as_bytes().as_ptr());
        wipe();
    }

    #[test]
    fn lru_evicts_least_recently_used_within_budget() {
        let mut lru: ByteLru<u32, u32> = ByteLru::new(10);
        lru.insert(1, 1, 4);
        lru.insert(2, 2, 4);
        assert_eq!(lru.get(&1), Some(1)); // 2 is now the oldest
        lru.insert(3, 3, 4);
        assert_eq!(lru.get(&2), None);
        assert_eq!(lru.get(&1), Some(1));
        assert_eq!(lru.get(&3), Some(3));
        assert_eq!(lru.used(), 8);
        lru.insert(4, 4, 11); // larger than the budget: not kept
        assert_eq!(lru.get(&4), None);
        assert_eq!(lru.used(), 8);
    }
}
