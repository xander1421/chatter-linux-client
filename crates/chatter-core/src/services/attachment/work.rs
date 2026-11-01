//! Shared admission for attachment CPU/file work. Acquire before spawning so a
//! batch queues small futures, never one decoder/allocation per selected photo.
use std::sync::{Arc, LazyLock};
use tokio::sync::Semaphore;

static WORK: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(1)));

/// Run attachment preparation away from the caller's executor (including a UI
/// executor). A running job owns admission even if its awaiting future is
/// cancelled: spawn_blocking cannot interrupt a decoder or filesystem call.
pub async fn run_attachment_work<F, T>(work: F) -> Result<T, tokio::task::JoinError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    run_with_gate(WORK.clone(), work).await
}

async fn run_with_gate<F, T>(gate: Arc<Semaphore>, work: F) -> Result<T, tokio::task::JoinError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let permit = gate
        .acquire_owned()
        .await
        .expect("attachment gate stays open");
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    // A current-thread runtime models an event loop: blocking work waits for a
    // release sent ONLY by that loop. Inline work would deadlock, not merely
    // exceed a timing threshold. A watchdog only bounds a broken test's hang.
    #[tokio::test(flavor = "current_thread")]
    async fn batch_work_yields_and_cancelled_waiters_never_start() {
        let gate = Arc::new(Semaphore::new(1));
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let first_gate = gate.clone();
        let first = tokio::spawn(async move {
            run_with_gate(first_gate, move || {
                entered_tx.send(()).unwrap();
                release_rx
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap();
                1
            })
            .await
            .unwrap()
        });
        entered_rx.await.unwrap();
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let next_ran = ran.clone();
        let next_gate = gate.clone();
        let next = tokio::spawn(async move {
            run_with_gate(next_gate, move || {
                next_ran.store(true, std::sync::atomic::Ordering::SeqCst);
            })
            .await
            .unwrap();
        });
        tokio::task::yield_now().await;
        assert_eq!(gate.available_permits(), 0);
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
        next.abort();
        assert!(next.await.unwrap_err().is_cancelled());
        // Cancelling a running await must not free admission before it exits.
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        assert_eq!(gate.available_permits(), 0);
        release_tx.send(()).unwrap();
        let permit = gate.acquire().await.unwrap();
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
        drop(permit);
        assert_eq!(
            run_with_gate(gate.clone(), || Err::<(), _>("bad photo"))
                .await
                .unwrap(),
            Err("bad photo")
        );
        assert!(
            run_with_gate(gate.clone(), || panic!("decoder panic"))
                .await
                .is_err()
        );
        assert_eq!(run_with_gate(gate, || 7).await.unwrap(), 7);
    }
}
