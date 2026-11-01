//! Ctrl-C and `kill`: the first signal stops new work and asks running
//! processes to finish (they get `general.shutdown_timeout`, then they're
//! killed); a second one kills them right away.

use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use tokio_util::sync::CancellationToken;

/// Shared by everything a command runs.
#[derive(Debug, Clone, Default)]
pub struct Shutdown {
    stop: CancellationToken,
    kill: CancellationToken,
    /// Exit code of the first signal: 130 (SIGINT) or 143 (SIGTERM).
    code: Arc<AtomicI32>,
}

/// Wait for SIGINT (130) or SIGTERM (143).
async fn next_signal(term: &mut tokio::signal::unix::Signal) -> i32 {
    tokio::select! {
        _ = tokio::signal::ctrl_c() => 130,
        _ = term.recv() => 143,
    }
}

impl Shutdown {
    /// Listen for SIGINT/SIGTERM in the background.
    pub fn listen() -> Self {
        let shutdown = Self::default();
        let handle = shutdown.clone();
        tokio::spawn(async move {
            let Ok(mut term) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            else {
                return;
            };
            let code = next_signal(&mut term).await;
            tracing::warn!("stopping; press Ctrl-C again to stop right away");
            handle.stop(code);
            next_signal(&mut term).await;
            tracing::warn!("stopping right away");
            handle.kill.cancel();
        });
        shutdown
    }

    /// Start stopping, as if a signal with `code` arrived.
    pub fn stop(&self, code: i32) {
        let _ = self
            .code
            .compare_exchange(0, code, Ordering::SeqCst, Ordering::SeqCst);
        self.stop.cancel();
    }

    /// Stop without waiting for anything (the second signal).
    pub fn kill(&self) {
        self.stop(130);
        self.kill.cancel();
    }

    pub fn is_stopping(&self) -> bool {
        self.stop.is_cancelled()
    }

    /// Resolves once stopping started.
    pub async fn stopping(&self) {
        self.stop.cancelled().await
    }

    /// Resolves once processes must be killed right away.
    pub async fn killing(&self) {
        self.kill.cancelled().await
    }

    /// The exit code to use, once stopping started.
    pub fn code(&self) -> Option<i32> {
        match self.code.load(Ordering::SeqCst) {
            0 => None,
            code => Some(code),
        }
    }
}
