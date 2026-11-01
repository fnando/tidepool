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
    /// A child's parent's code: a signal stops the whole process.
    parent_code: Option<Arc<AtomicI32>>,
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

    /// A shutdown for one part of the process (a daemon service): it stops
    /// when this one does, and can also be stopped on its own.
    pub fn child(&self) -> Self {
        Self {
            stop: self.stop.child_token(),
            kill: self.kill.child_token(),
            code: Arc::default(),
            parent_code: Some(self.code.clone()),
        }
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
        let parent = || self.parent_code.as_ref().map(|c| c.load(Ordering::SeqCst));
        match self.code.load(Ordering::SeqCst) {
            0 => parent().filter(|c| *c != 0),
            code => Some(code),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn children_stop_with_the_parent_or_on_their_own() {
        let parent = Shutdown::default();
        let (a, b) = (parent.child(), parent.child());
        a.stop(143);
        assert!(a.is_stopping() && !b.is_stopping() && !parent.is_stopping());
        assert_eq!((a.code(), b.code()), (Some(143), None));
        parent.stop(130);
        assert!(b.is_stopping());
        assert_eq!(b.code(), Some(130), "the signal's code");
        assert_eq!(a.code(), Some(143), "its own stop came first");
    }
}
