//! Shared GIF-only workers. Pending attachment ids coalesce duplicate requests.
use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Condvar, LazyLock, Mutex};

type Job = (String, Box<dyn FnOnce() + Send>);
#[derive(Default)]
struct Queue {
    jobs: VecDeque<Job>,
    pending: HashSet<String>,
}
static POOL: LazyLock<Arc<(Mutex<Queue>, Condvar)>> = LazyLock::new(|| {
    let pool = Arc::new((Mutex::new(Queue::default()), Condvar::new()));
    for _ in 0..2 {
        let pool = pool.clone();
        std::thread::spawn(move || loop {
            let (lock, wake) = &*pool;
            let (id, job) = {
                let mut queue = lock.lock().unwrap_or_else(|p| p.into_inner());
                while queue.jobs.is_empty() {
                    queue = wake.wait(queue).unwrap_or_else(|p| p.into_inner());
                }
                queue.jobs.pop_front().unwrap()
            };
            // A malformed GIF must not permanently kill a pool worker.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
            lock.lock()
                .unwrap_or_else(|p| p.into_inner())
                .pending
                .remove(&id);
        });
    }
    pool
});

/// Submit background work, promoting visible attachments ahead of pending work.
/// There are at most two running decodes and 128 waiting jobs.
pub fn submit(id: String, visible: bool, job: impl FnOnce() + Send + 'static) {
    let (lock, wake) = &**POOL;
    let mut queue = lock.lock().unwrap_or_else(|p| p.into_inner());
    if queue.pending.contains(&id) {
        if visible {
            if let Some(index) = queue.jobs.iter().position(|(key, _)| key == &id) {
                let job = queue.jobs.remove(index).unwrap();
                queue.jobs.push_front(job);
            }
        }
        return;
    }
    if queue.jobs.len() >= 128 {
        if !visible {
            return;
        }
        if let Some((evicted, _)) = queue.jobs.pop_back() {
            queue.pending.remove(&evicted);
        }
    }
    queue.pending.insert(id.clone());
    if visible {
        queue.jobs.push_front((id, Box::new(job)));
    } else {
        queue.jobs.push_back((id, Box::new(job)));
    }
    wake.notify_one();
}
