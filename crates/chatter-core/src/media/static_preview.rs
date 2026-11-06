//! Bounded, cancellable static previews. Encoded originals never reach list rendering.
use super::performance_trace as trace;
use crate::{client::ChatterClient, domain::ids::AttachmentId};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
};

static WORKERS: std::sync::LazyLock<Arc<tokio::sync::Semaphore>> =
    std::sync::LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(3)));

// A byte budget alone admits arbitrarily many tiny previews and makes each
// LRU/accounting scan grow with chat history. Bound retained identities too.
const MAX_CACHE_ENTRIES: usize = 64;

static GENERATION: AtomicU64 = AtomicU64::new(0);
static CACHES: std::sync::LazyLock<Mutex<Vec<Weak<Mutex<Cache>>>>> =
    std::sync::LazyLock::new(|| Mutex::new(Vec::new()));

/// Participate in the application's existing logout, lock and expiry wipes.
/// Advancing the generation also revokes in-flight jobs before cache insertion.
pub fn wipe() {
    let _trace = trace::section(c"chatter.media.static.wipe");
    GENERATION.fetch_add(1, Ordering::SeqCst);
    if let Ok(mut caches) = CACHES.lock() {
        caches.retain(|cache| {
            if let Some(cache) = cache.upgrade() {
                if let Ok(mut cache) = cache.lock() {
                    cache.clear();
                    trace::counter(c"chatter.media.static.cache_entries", 0);
                }
                true
            } else {
                false
            }
        });
    }
}

#[derive(Clone)]
pub struct Preview {
    pub rgba: Arc<[u8]>,
    pub width: u32,
    pub height: u32,
}

/// A loader belongs to one authenticated UI session; dropping it drops its cache.
pub struct StaticPreviewLoader {
    workers: Arc<tokio::sync::Semaphore>,
    cache: Arc<Mutex<Cache>>,
    generation: u64,
    tick: std::sync::atomic::AtomicU64,
}
impl Default for StaticPreviewLoader {
    fn default() -> Self {
        let cache = Arc::new(Mutex::new(HashMap::new()));
        if let Ok(mut caches) = CACHES.lock() {
            caches.retain(|cache| cache.strong_count() > 0);
            caches.push(Arc::downgrade(&cache));
        }
        Self {
            workers: WORKERS.clone(),
            cache,
            generation: GENERATION.load(Ordering::SeqCst),
            tick: std::sync::atomic::AtomicU64::new(0),
        }
    }
}
impl StaticPreviewLoader {
    pub fn is_current(&self) -> bool {
        self.generation == GENERATION.load(Ordering::SeqCst)
    }
    /// Caller cancels when the item leaves the viewport. Recheck after each I/O
    /// boundary and before publication; an executing codec cannot be interrupted.
    pub async fn load(
        self: Arc<Self>,
        client: Arc<ChatterClient>,
        id: AttachmentId,
        edge: u32,
        cancelled: tokio_util::sync::CancellationToken,
    ) -> Option<Preview> {
        self.load_traced(client, id, edge, cancelled, trace::Job::new())
            .await
    }

    /// Same loader with a content-free token connecting UI demand to workers.
    pub async fn load_traced(
        self: Arc<Self>,
        client: Arc<ChatterClient>,
        id: AttachmentId,
        edge: u32,
        cancelled: tokio_util::sync::CancellationToken,
        job: trace::Job,
    ) -> Option<Preview> {
        let _load = job.span(c"chatter.media.static.load");
        let edge = edge.clamp(1, 768);
        let key = (id, edge);
        let current = || {
            let current = !cancelled.is_cancelled() && self.is_current();
            if !current {
                job.event(if cancelled.is_cancelled() {
                    c"chatter.media.static.cancelled"
                } else {
                    c"chatter.media.static.generation_invalid"
                });
            }
            current
        };
        if !current() {
            return None;
        }
        let tick = self.tick.fetch_add(1, Ordering::Relaxed);
        {
            let _lookup = trace::section(c"chatter.media.static.cache_lookup");
            if let Some((value, age)) = self.cache.lock().ok()?.get_mut(&key) {
                *age = tick;
                job.event(c"chatter.media.static.cache_hit");
                return Some(value.clone());
            }
        }
        job.event(c"chatter.media.static.cache_miss");
        let wait = job.span(c"chatter.media.static.semaphore_wait");
        let permit = acquire(self.workers.clone(), &cancelled).await;
        drop(wait);
        let Some(permit) = permit else {
            job.event(c"chatter.media.static.wait_cancelled");
            return None;
        };
        let held = job.span(c"chatter.media.static.permit_held");
        if trace::enabled() {
            trace::counter(
                c"chatter.media.static.available_permits_at_acquire",
                self.workers.available_permits() as i64,
            );
        }
        if !current() {
            return None;
        }
        let session = client.session_guard();
        let att = {
            let _metadata = trace::section(c"chatter.media.static.metadata");
            client.get_attachment_metadata(&id)?
        };
        if !att.metadata.is_image() || att.metadata.is_gif() {
            return None;
        }
        // Reuse bounded, authenticated sidecar/original preparation. This keeps
        // source allocation limits, durable derived thumbnails and session fences.
        let sidecar = job.span(c"chatter.media.static.sidecar");
        let preview = tokio::select! {
            _ = cancelled.cancelled() => {
                job.event(c"chatter.media.static.cancelled");
                return None;
            },
            preview = trace::scope(job, client.ensure_attachment_preview(id, session.clone())) => preview,
        };
        drop(sidecar);
        let preview = match preview {
            Some(preview) => preview,
            None => {
                let _fallback = job.span(c"chatter.media.static.local_prepare");
                tokio::select! {
                    _ = cancelled.cancelled() => {
                        job.event(c"chatter.media.static.cancelled");
                        return None;
                    },
                    preview = trace::scope(job, client.prepare_downloaded_image_preview(id, session.clone())) => preview.ok()?,
                }
            }
        };
        if !current() || !session.is_current() {
            job.event(c"chatter.media.static.publication_invalid");
            return None;
        }
        let loader = self.clone();
        let queued = job.span(c"chatter.media.static.resize_queue");
        tokio::task::spawn_blocking(move || {
            let _context = job.enter();
            drop(queued);
            let _worker = job.span(c"chatter.media.static.resize_worker");
            let _held = held;
            let _permit = permit;
            if cancelled.is_cancelled() || !session.is_current() || !loader.is_current() {
                job.event(c"chatter.media.static.resize_invalid");
                return None;
            }
            let image = {
                let _resize = trace::section(c"chatter.media.static.resize");
                fit_pixels(preview.pixels, edge)
            };
            let result = Preview {
                width: image.width(),
                height: image.height(),
                rgba: image.into_raw().into(),
            };
            let _insert = trace::section(c"chatter.media.static.cache_insert");
            let mut cache = loader.cache.lock().ok()?;
            if cancelled.is_cancelled() || !session.is_current() || !loader.is_current() {
                job.event(c"chatter.media.static.insert_invalid");
                return None;
            }
            remember(&mut cache, key, result.clone(), tick, 32 * 1024 * 1024);
            trace::counter(c"chatter.media.static.cache_entries", cache.len() as i64);
            Some(result)
        })
        .await
        .ok()
        .flatten()
    }
}

/// Preserve native pixels when already within the bound. In particular, do not
/// inflate a 160/320px sidecar into a 640px RAM/GPU copy without adding detail.
fn fit_pixels(pixels: image::RgbaImage, edge: u32) -> image::RgbaImage {
    if pixels.width() <= edge && pixels.height() <= edge {
        pixels
    } else {
        image::DynamicImage::ImageRgba8(pixels)
            .thumbnail(edge, edge)
            .into_rgba8()
    }
}

async fn acquire(
    workers: Arc<tokio::sync::Semaphore>,
    cancelled: &tokio_util::sync::CancellationToken,
) -> Option<tokio::sync::OwnedSemaphorePermit> {
    tokio::select! {
        _ = cancelled.cancelled() => None,
        permit = workers.acquire_owned() => permit.ok(),
    }
}

#[cfg(test)]
fn decode(bytes: &[u8], edge: u32) -> Option<Preview> {
    crate::services::attachment::validate_media(bytes).ok()?;
    let image = fit_pixels(super::ImageProcessor::load_from_memory(bytes).ok()?.into_rgba8(), edge);
    Some(Preview {
        width: image.width(),
        height: image.height(),
        rgba: image.into_raw().into(),
    })
}

type Cache = HashMap<(AttachmentId, u32), (Preview, u64)>;
fn remember(
    cache: &mut Cache,
    key: (AttachmentId, u32),
    result: Preview,
    tick: u64,
    budget: usize,
) {
    if result.rgba.len() > budget {
        return;
    }
    cache.remove(&key);
    let mut used: usize = cache.values().map(|(v, _)| v.rgba.len()).sum();
    while cache.len() >= MAX_CACHE_ENTRIES || used + result.rgba.len() > budget {
        let Some(oldest) = cache
            .iter()
            .min_by_key(|(_, (_, age))| *age)
            .map(|(key, _)| *key)
        else {
            break;
        };
        let eviction = trace::section(c"chatter.media.static.cache_evict");
        if let Some((value, _)) = cache.remove(&oldest) {
            used -= value.rgba.len();
        }
        drop(eviction);
    }
    cache.insert(key, (result, tick));
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn small_preview_keeps_native_pixels_and_allocation() {
        let pixels = image::RgbaImage::from_pixel(160, 80, image::Rgba([3, 41, 220, 127]));
        let allocation = pixels.as_ptr();
        let pixels = fit_pixels(pixels, 640);
        assert_eq!(pixels.dimensions(), (160, 80));
        assert_eq!(pixels.as_ptr(), allocation);
        assert!(pixels.pixels().all(|p| p.0 == [3, 41, 220, 127]));
    }
    #[test]
    fn lifecycle_wipe_releases_pixels_and_revokes_old_loaders() {
        let loader = StaticPreviewLoader::default();
        let preview = Preview {
            rgba: vec![0; 16].into(),
            width: 2,
            height: 2,
        };
        loader
            .cache
            .lock()
            .unwrap()
            .insert((AttachmentId::new(), 640), (preview, 0));
        wipe();
        assert!(!loader.is_current());
        assert!(loader.cache.lock().unwrap().is_empty());
        assert!(StaticPreviewLoader::default().is_current());
    }
    #[tokio::test]
    async fn offscreen_request_does_not_wait_for_busy_workers() {
        let workers = Arc::new(tokio::sync::Semaphore::new(0));
        let cancelled = tokio_util::sync::CancellationToken::new();
        let cancel = cancelled.clone();
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            cancel.cancel();
        });
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            acquire(workers, &cancelled),
        )
        .await;
        assert!(result.unwrap().is_none());
    }
    #[test]
    fn preview_fits_display_without_stretching() {
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(2400, 1200)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        let preview = decode(bytes.get_ref(), 640).unwrap();
        assert_eq!((preview.width, preview.height), (640, 320));
        assert_eq!(preview.rgba.len(), 640 * 320 * 4);
        assert!(decode(b"broken", 640).is_none());
    }
    #[test]
    fn cache_is_byte_bounded_and_distinguishes_target_sizes() {
        let mut cache = Cache::new();
        let id = AttachmentId::new();
        let preview = Preview {
            rgba: vec![0; 16].into(),
            width: 2,
            height: 2,
        };
        remember(&mut cache, (id, 320), preview.clone(), 1, 32);
        remember(&mut cache, (id, 640), preview.clone(), 2, 32);
        assert!(Arc::ptr_eq(&cache[&(id, 640)].0.rgba, &preview.rgba));
        let next = AttachmentId::new();
        remember(&mut cache, (next, 640), preview, 3, 32);
        assert!(!cache.contains_key(&(id, 320)));
        assert!(cache.contains_key(&(id, 640)));
        assert_eq!(cache.values().map(|(v, _)| v.rgba.len()).sum::<usize>(), 32);
    }

    #[test]
    fn tiny_previews_have_a_count_bound_and_keep_recent_entries() {
        let mut cache = Cache::new();
        let ids: Vec<_> = (0..MAX_CACHE_ENTRIES).map(|_| AttachmentId::new()).collect();
        let preview = Preview { rgba: vec![0;4].into(), width: 1, height: 1 };
        for (tick, id) in ids.iter().enumerate() {
            remember(&mut cache, (*id,640), preview.clone(), tick as u64, 4096);
        }
        remember(&mut cache, (ids[0],640), preview.clone(), 1000, 4096);
        remember(&mut cache, (AttachmentId::new(),640), preview, 1001, 4096);
        assert_eq!(cache.len(), MAX_CACHE_ENTRIES);
        assert!(cache.contains_key(&(ids[0],640)));
        assert!(!cache.contains_key(&(ids[1],640)));
    }
}
