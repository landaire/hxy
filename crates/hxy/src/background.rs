//! Process-wide CPU-bound worker pool.
//!
//! Replaces ad-hoc `std::thread::spawn` calls for template
//! parse/execute, entropy compute, binary diff, and the
//! ImHex-Patterns download. Plugin operations stay on their own
//! per-op threads (see `crate::plugins::runner`) because they can
//! block on plugin I/O for arbitrarily long and shouldn't head-of-line
//! block the rest of the work.
//!
//! Submitted jobs are self-contained closures. Any result delivery
//! back to the UI is the closure's responsibility, typically via an
//! `egui_inbox::UiInbox` or `std::sync::mpsc` channel captured into
//! the closure.
//!
//! `wasm32-unknown-unknown` has no `std::thread`, so the wasm
//! impl runs the job inline on the calling thread. Long jobs
//! (file-wide entropy, multi-MB checksum) freeze the frame for
//! their duration there -- the eventual fix is a Web Worker
//! variant, but for now correctness over latency.

/// Run `job` on the shared worker pool (or inline on wasm).
/// Returns immediately on desktop; on wasm runs synchronously
/// before returning. The closure is responsible for delivering
/// any result back to the UI through a captured channel.
#[cfg(not(target_arch = "wasm32"))]
pub fn submit<F>(job: F)
where
    F: FnOnce() + Send + 'static,
{
    desktop::pool().dispatch(Box::new(job));
}

#[cfg(target_arch = "wasm32")]
pub fn submit<F>(job: F)
where
    F: FnOnce() + 'static,
{
    job();
}

/// Force lazy init of the pool so the workers exist before the
/// first job arrives instead of being created on the hot path of
/// the first template run. No-op on wasm.
#[cfg(not(target_arch = "wasm32"))]
pub fn init() {
    let _ = desktop::pool();
}

#[cfg(target_arch = "wasm32")]
pub fn init() {}

#[cfg(not(target_arch = "wasm32"))]
mod desktop {
    use std::sync::OnceLock;
    use std::thread;

    use crossbeam_channel::Sender;
    use crossbeam_channel::unbounded;

    type Job = Box<dyn FnOnce() + Send + 'static>;

    static POOL: OnceLock<BackgroundPool> = OnceLock::new();

    /// Long-lived worker count. Two is enough to keep one slot
    /// free for short jobs (entropy, diff) while a long job
    /// (template parse over a large file, patterns download) is
    /// running on the other.
    const DEFAULT_WORKERS: usize = 2;

    pub(super) struct BackgroundPool {
        tx: Sender<Job>,
    }

    impl BackgroundPool {
        fn new(workers: usize) -> Self {
            assert!(workers > 0, "background pool needs at least one worker");
            let (tx, rx) = unbounded::<Job>();
            for i in 0..workers {
                let rx = rx.clone();
                thread::Builder::new()
                    .name(format!("hxy-bg-{i}"))
                    .spawn(move || {
                        while let Ok(job) = rx.recv() {
                            job();
                        }
                    })
                    .expect("spawn background worker");
            }
            Self { tx }
        }

        pub(super) fn dispatch(&self, job: Job) {
            // Send only fails if every worker has dropped its
            // receiver, which only happens if all of them
            // panicked. Drop the job silently; the caller's
            // wait-for-result path times out the same way it
            // would for a worker that died mid-job.
            let _ = self.tx.send(job);
        }
    }

    pub(super) fn pool() -> &'static BackgroundPool {
        POOL.get_or_init(|| BackgroundPool::new(DEFAULT_WORKERS))
    }
}
