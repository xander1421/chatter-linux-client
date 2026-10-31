//! Bounded, cancellable static previews. Encoded originals never reach list rendering.
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

static GENERATION: AtomicU64 = AtomicU64::new(0);
static CACHES: std::sync::LazyLock<Mutex<Vec<Weak<Mutex<Cache>>>>> =
    std::sync::LazyLock::new(|| Mutex::new(Vec::new()));

/// Participate in the application's existing logout, lock and expiry wipes.
/// Advancing the generation also revokes in-flight jobs before cache insertion.
pub fn wipe() {
    GENERATION.fetch_add(1, Ordering::SeqCst);
    if let Ok(mut caches) = CACHES.lock() {
        caches.retain(|cache| {
            if let Some(cache) = cache.upgrade() {
                if let Ok(mut cache) = cache.lock() {
                    cache.clear();
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
        let edge = edge.clamp(1, 768);
        let key = (id, edge);
        let current = || !cancelled.is_cancelled() && self.is_current();
        if !current() {
            return None;
        }
        let tick = self.tick.fetch_add(1, Ordering::Relaxed);
        if let Some((value, age)) = self.cache.lock().ok()?.get_mut(&key) {
            *age = tick;
            return Some(value.clone());
        }
        let permit = acquire(self.workers.clone(), &cancelled).await?;
        if !current() {
            return None;
        }
        let session = client.session_guard();
        let att = client.get_attachment_metadata(&id)?;
        if !att.metadata.is_image() || att.metadata.is_gif() {
            return None;
        }
        // Reuse bounded, authenticated sidecar/original preparation. This keeps
        // source allocation limits, durable derived thumbnails and session fences.
        let preview = tokio::select! {
            _ = cancelled.cancelled() => return None,
            preview = client.ensure_attachment_preview(id, session.clone()) => preview,
        };
        let preview = match preview {
            Some(preview) => preview,
            None => tokio::select! {
                _ = cancelled.cancelled() => return None,
                preview = client.prepare_downloaded_image_preview(id, session.clone()) => preview.ok()?,
            },
        };
        if !current() || !session.is_current() {
            return None;
        }
        let loader = self.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            if cancelled.is_cancelled() || !session.is_current() || !loader.is_current() {
                return None;
            }
            let image = image::DynamicImage::ImageRgba8(preview.pixels)
                .thumbnail(edge, edge)
                .into_rgba8();
            let result = Preview {
                width: image.width(),
                height: image.height(),
                rgba: image.into_raw().into(),
            };
            let mut cache = loader.cache.lock().ok()?;
            if cancelled.is_cancelled() || !session.is_current() || !loader.is_current() {
                return None;
            }
            remember(&mut cache, key, result.clone(), tick, 32 * 1024 * 1024);
            Some(result)
        })
        .await
        .ok()
        .flatten()
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
    let image = super::ImageProcessor::load_from_memory(bytes)
        .ok()?
        .thumbnail(edge, edge)
        .to_rgba8();
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
    while used + result.rgba.len() > budget {
        let Some(oldest) = cache
            .iter()
            .min_by_key(|(_, (_, age))| *age)
            .map(|(key, _)| *key)
        else {
            break;
        };
        if let Some((value, _)) = cache.remove(&oldest) {
            used -= value.rgba.len();
        }
    }
    cache.insert(key, (result, tick));
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
