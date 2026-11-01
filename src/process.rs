//! Running yt-dlp, ffmpeg and hooks: each in its own process group, so
//! stopping one also stops what it started (yt-dlp runs ffmpeg), and only
//! tidepool decides when they stop (a Ctrl-C in the terminal reaches tidepool,
//! not them).

use std::process::{ExitStatus, Output, Stdio};
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

use crate::shutdown::Shutdown;

/// Why a process didn't finish on its own.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Spawn(std::io::Error),
    #[error("stopped")]
    Stopped,
    #[error("{0}")]
    Wait(std::io::Error),
}

/// Put a command in its own process group.
pub fn isolate(command: &mut Command) -> &mut Command {
    command.process_group(0).kill_on_drop(true)
}

/// Signal a child's whole process group.
fn signal(child: &Child, signal: libc::c_int) {
    if let Some(pid) = child.id() {
        // SAFETY: signalling the process group of a child we spawned with
        // `process_group(0)`, so its group id is its pid.
        unsafe {
            libc::kill(-(pid as libc::pid_t), signal);
        }
    }
}

/// Stop a child: SIGTERM to its group, then SIGKILL after `grace`, or right
/// away once `shutdown` says so. Returns once it exited.
pub async fn stop(child: &mut Child, grace: Duration, shutdown: &Shutdown) {
    signal(child, libc::SIGTERM);
    tokio::select! {
        _ = child.wait() => return,
        _ = tokio::time::sleep(grace) => {}
        _ = shutdown.killing() => {}
    }
    signal(child, libc::SIGKILL);
    let _ = child.kill().await;
}

/// Wait for a child; when stopping starts, stop it (see [`stop`]).
pub async fn wait(
    child: &mut Child,
    grace: Duration,
    shutdown: &Shutdown,
) -> Result<ExitStatus, Error> {
    tokio::select! {
        status = child.wait() => status.map_err(Error::Wait),
        _ = shutdown.stopping() => {
            stop(child, grace, shutdown).await;
            Err(Error::Stopped)
        }
    }
}

/// Run a command to completion, collecting its output, unless stopping
/// starts first.
pub async fn output(
    command: &mut Command,
    grace: Duration,
    shutdown: &Shutdown,
) -> Result<Output, Error> {
    if shutdown.is_stopping() {
        return Err(Error::Stopped);
    }
    let mut child = isolate(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(Error::Spawn)?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let out = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf).await;
        buf
    });
    let err = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf).await;
        buf
    });
    let status = wait(&mut child, grace, shutdown).await?;
    Ok(Output {
        status,
        stdout: out.await.unwrap_or_default(),
        stderr: err.await.unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn sh(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    fn alive(pid: i32) -> bool {
        // SAFETY: signal 0 only checks that the process exists.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[tokio::test]
    async fn collects_output() {
        let out = output(
            &mut sh("echo out; echo err >&2"),
            Duration::from_secs(1),
            &Shutdown::default(),
        )
        .await
        .unwrap();
        assert!(out.status.success());
        assert_eq!(
            (out.stdout, out.stderr),
            (b"out\n".to_vec(), b"err\n".to_vec())
        );
    }

    #[tokio::test]
    async fn stopping_ends_the_whole_group() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        // A child that starts a grandchild (like yt-dlp starting ffmpeg).
        let mut command = sh(&format!(
            "sleep 30 & echo $! > {}; wait",
            pid_file.display()
        ));
        let shutdown = Shutdown::default();
        let stopper = shutdown.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            stopper.stop(130);
        });
        let started = Instant::now();
        let result = output(&mut command, Duration::from_secs(5), &shutdown).await;
        assert!(matches!(result, Err(Error::Stopped)));
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "SIGTERM was enough"
        );
        let grandchild: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!alive(grandchild), "the grandchild was stopped too");
    }

    #[tokio::test]
    async fn waits_out_the_grace_period_unless_killed() {
        // Ignores SIGTERM (so does the sleep it starts): only SIGKILL works.
        let stubborn = "trap '' TERM; sleep 30";
        let shutdown = Shutdown::default();
        shutdown.stop(130);
        // Already stopping: nothing starts.
        assert!(matches!(
            output(&mut sh(stubborn), Duration::from_secs(1), &shutdown).await,
            Err(Error::Stopped)
        ));

        let shutdown = Shutdown::default();
        let mut child = isolate(&mut sh(stubborn)).spawn().unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let started = Instant::now();
        stop(&mut child, Duration::from_millis(500), &shutdown).await;
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_millis(500) && waited < Duration::from_secs(3),
            "{waited:?}"
        );

        // A second signal doesn't wait for the grace period.
        let shutdown = Shutdown::default();
        let mut child = isolate(&mut sh(stubborn)).spawn().unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let killer = shutdown.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            killer.kill();
        });
        let started = Instant::now();
        stop(&mut child, Duration::from_secs(30), &shutdown).await;
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
