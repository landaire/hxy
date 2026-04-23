//! Background save task, adapted from lantia-locator. Keeps the UI thread
//! free of any database I/O by sharing state behind `Arc<RwLock<>>` and
//! waking the task via a `Notify` whenever the UI mutates something.

use std::sync::Arc;
use std::time::Duration;

use sqlx::SqlitePool;
use tokio::sync::Notify;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::warn;

use crate::persist::store_app_settings;
use crate::persist::store_window_settings;
use crate::state::SharedPersistedState;

/// Handle returned by [`spawn_save_task`]. Dropping it does NOT stop the
/// task; call [`SaveHandle::shutdown`] for a graceful final save.
pub struct SaveHandle {
    pub notify: Arc<Notify>,
    shutdown: Option<oneshot::Sender<()>>,
    join: Option<JoinHandle<()>>,
}

impl SaveHandle {
    /// Request a final save and wait for the task to exit. Callers should
    /// invoke this from an async context during shutdown.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.await;
        }
    }
}

/// Spawn a task that debounces saves: any call to `notify.notify_one()`
/// schedules a save after a short delay so rapid updates coalesce. Also
/// saves once per `interval` as a backstop.
pub fn spawn_save_task(pool: SqlitePool, state: SharedPersistedState, interval: Duration) -> SaveHandle {
    let notify = Arc::new(Notify::new());
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    let task_notify = Arc::clone(&notify);
    let join = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticker.tick() => save_once(&pool, &state).await,
                _ = task_notify.notified() => {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    save_once(&pool, &state).await;
                }
                _ = &mut shutdown_rx => {
                    save_once(&pool, &state).await;
                    break;
                }
            }
        }
    });
    SaveHandle { notify, shutdown: Some(shutdown_tx), join: Some(join) }
}

async fn save_once(pool: &SqlitePool, state: &SharedPersistedState) {
    let (window, app) = {
        let s = state.read();
        (s.window, s.app.clone())
    };
    if let Err(e) = store_window_settings(pool, &window).await {
        warn!(error = %e, "persist window settings");
    }
    if let Err(e) = store_app_settings(pool, &app).await {
        warn!(error = %e, "persist app settings");
    }
}
