//! Running yt-dlp for one video.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_util::sync::CancellationToken;

use super::info::VideoInfo;
use crate::config::merge::EffectiveSource;

/// The watch URL for a video id.
pub fn watch_url(video_id: &str) -> String {
    format!("https://www.youtube.com/watch?v={video_id}")
}

/// Arguments to download one video into `tmp`.
pub fn download_args(
    source: &EffectiveSource,
    video_id: &str,
    tmp: &Path,
    cookies: Option<&Path>,
) -> Vec<String> {
    let mut args = vec!["-f".to_string(), source.format.clone()];
    if !source.merge_output_format.is_empty() {
        args.extend([
            "--merge-output-format".into(),
            source.merge_output_format.clone(),
        ]);
    }
    args.extend([
        "-o".into(),
        format!("{}/media.%(ext)s", tmp.display()),
        "--write-info-json".into(),
        "-o".into(),
        format!("infojson:{}/ytdlp", tmp.display()),
        "--no-write-playlist-metafiles".into(),
        "--no-playlist".into(),
        "--no-update".into(),
        "--newline".into(),
        "--progress".into(),
        "--progress-template".into(),
        "download:%(progress)j".into(),
        "--print".into(),
        "after_move:filepath".into(),
    ]);
    if let Some(cookies) = cookies {
        args.extend(["--cookies".into(), cookies.display().to_string()]);
    }
    args.extend(source.ytdlp_args.iter().cloned());
    args.extend(["--".into(), watch_url(video_id)]);
    args
}

/// Why a download failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// Retrying won't help (private, removed, members-only, age-gated).
    pub permanent: bool,
    pub message: String,
}

/// The useful line of yt-dlp's stderr, and whether the failure is permanent.
pub fn classify(stderr: &str) -> Failure {
    let line = stderr
        .lines()
        .rev()
        .find(|l| l.starts_with("ERROR:"))
        .or_else(|| stderr.lines().rev().find(|l| !l.trim().is_empty()))
        .unwrap_or("yt-dlp failed without an error message")
        .trim();
    // "ERROR: [youtube] <id>: message" → "message"
    let message = line.strip_prefix("ERROR:").map(str::trim).unwrap_or(line);
    let message = match message.strip_prefix('[').and_then(|m| m.split_once("] ")) {
        Some((_, rest)) => rest.split_once(": ").map_or(rest, |(_, m)| m),
        None => message,
    };
    const PERMANENT: [&str; 8] = [
        "Video unavailable",
        "Private video",
        "This video has been removed",
        "members-only",
        "Join this channel",
        "Sign in to confirm your age",
        "account associated with this video has been terminated",
        "This video is no longer available",
    ];
    Failure {
        permanent: PERMANENT.iter().any(|p| line.contains(p)),
        message: message.to_string(),
    }
}

/// Reports yt-dlp's progress every 10%, per file (video and audio are
/// separate downloads before merging).
#[derive(Default)]
struct Progress {
    file: Option<String>,
    step: Option<u64>,
}

impl Progress {
    fn update(&mut self, line: &str, label: &str) {
        let Ok(p) = serde_json::from_str::<serde_json::Value>(line) else {
            return;
        };
        let downloaded = p["downloaded_bytes"].as_f64().unwrap_or(0.0);
        let Some(total) = p["total_bytes"]
            .as_f64()
            .or_else(|| p["total_bytes_estimate"].as_f64())
            .filter(|t| *t > 0.0)
        else {
            return;
        };
        let file = p["filename"].as_str().map(String::from);
        if file != self.file {
            self.file = file;
            self.step = None;
        }
        let percent = ((downloaded / total) * 100.0).clamp(0.0, 100.0) as u64;
        let step = percent / 10;
        if self.step.is_some_and(|s| step <= s) {
            return;
        }
        self.step = Some(step);
        let mut line = format!(
            "{label}: {percent}% of {}",
            super::human_bytes(total as u64)
        );
        if let Some(speed) = p["speed"].as_f64().filter(|s| *s > 0.0) {
            line.push_str(&format!(" at {}/s", super::human_bytes(speed as u64)));
        }
        if let Some(eta) = p["eta"].as_u64().filter(|_| percent < 100) {
            line.push_str(&format!(", {}:{:02} left", eta / 60, eta % 60));
        }
        tracing::debug!("{line}");
    }
}

/// Stop a child: SIGTERM, then SIGKILL after `grace`; returns once it exited.
async fn stop(child: &mut tokio::process::Child, grace: Duration) {
    if let Some(pid) = child.id() {
        // SAFETY: sending a signal to a child process we spawned and still own.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
        if tokio::time::timeout(grace, child.wait()).await.is_ok() {
            return;
        }
    }
    let _ = child.kill().await;
}

fn cancelled() -> Failure {
    Failure {
        permanent: false,
        message: "cancelled".into(),
    }
}

/// Run yt-dlp with `args`; returns the final media path. When `cancel`
/// fires, yt-dlp is stopped (SIGTERM, then SIGKILL after `grace`) and this
/// returns only after it exited, so nothing writes to the work area anymore.
pub async fn download(
    ytdlp: &crate::tools::Ytdlp,
    args: &[String],
    label: &str,
    cancel: &CancellationToken,
    grace: Duration,
) -> Result<PathBuf, Failure> {
    tracing::trace!("yt-dlp command: {} {}", ytdlp.display(), args.join(" "));
    let mut child = ytdlp
        .command()
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| Failure {
            permanent: false,
            message: format!("cannot run {}: {e}", ytdlp.display()),
        })?;

    let stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let stderr_task = tokio::spawn(async move {
        let mut buf = String::new();
        let _ = tokio::io::AsyncReadExt::read_to_string(&mut stderr, &mut buf).await;
        buf
    });

    let mut lines = BufReader::new(stdout).lines();
    let mut progress = Progress::default();
    let mut final_path = None;
    loop {
        let line = tokio::select! {
            line = lines.next_line() => line,
            _ = cancel.cancelled() => {
                stop(&mut child, grace).await;
                return Err(cancelled());
            }
        };
        let Ok(Some(line)) = line else { break };
        let line = line.trim();
        if line.starts_with('{') {
            tracing::trace!("yt-dlp progress: {line}");
            progress.update(line, label);
        } else if !line.is_empty() {
            final_path = Some(PathBuf::from(line));
        }
    }
    let status = tokio::select! {
        status = child.wait() => status.map_err(|e| Failure { permanent: false, message: e.to_string() })?,
        _ = cancel.cancelled() => {
            stop(&mut child, grace).await;
            return Err(cancelled());
        }
    };
    let stderr = stderr_task.await.unwrap_or_default();
    for line in stderr.lines().filter(|l| !l.trim().is_empty()) {
        tracing::trace!("yt-dlp: {line}");
    }
    if !status.success() {
        return Err(classify(&stderr));
    }
    final_path.ok_or_else(|| Failure {
        permanent: false,
        message: "yt-dlp didn't report the downloaded file".into(),
    })
}

/// Full details for one video (`yt-dlp -J`), used for decisions that need
/// fields the index didn't have.
pub async fn video_info(
    ytdlp: &crate::tools::Ytdlp,
    video_id: &str,
    cookies: Option<&Path>,
) -> Result<VideoInfo, Failure> {
    let mut args = vec![
        "-J".to_string(),
        "--no-playlist".into(),
        "--no-update".into(),
        "--no-warnings".into(),
    ];
    if let Some(cookies) = cookies {
        args.extend(["--cookies".into(), cookies.display().to_string()]);
    }
    args.extend(["--".into(), watch_url(video_id)]);
    let output = ytdlp
        .command()
        .args(&args)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| Failure {
            permanent: false,
            message: format!("cannot run {}: {e}", ytdlp.display()),
        })?;
    if !output.status.success() {
        return Err(classify(&String::from_utf8_lossy(&output.stderr)));
    }
    VideoInfo::parse(&String::from_utf8_lossy(&output.stdout)).map_err(|message| Failure {
        permanent: false,
        message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::config::merge::effective_sources;

    fn source(extra: &str) -> EffectiveSource {
        let config: Config = toml::from_str(&format!(
            "[sources.lofi]\nurl = 'https://www.youtube.com/playlist?list=PLabc'\n{extra}"
        ))
        .unwrap();
        effective_sources(&config).remove(0)
    }

    #[test]
    fn builds_download_arguments() {
        let s = source("format = 'ba/b'\nytdlp_args = ['-x', '--audio-format', 'opus']");
        let args = download_args(
            &s,
            "-abc",
            Path::new("/r/.tmp/x"),
            Some(Path::new("/c.txt")),
        );
        assert_eq!(
            args.join(" "),
            "-f ba/b --merge-output-format mkv -o /r/.tmp/x/media.%(ext)s --write-info-json \
             -o infojson:/r/.tmp/x/ytdlp --no-write-playlist-metafiles --no-playlist --no-update --newline \
             --progress --progress-template download:%(progress)j --print after_move:filepath \
             --cookies /c.txt -x --audio-format opus -- https://www.youtube.com/watch?v=-abc"
        );
    }

    #[test]
    fn classifies_failures() {
        let f = classify(
            "WARNING: x\nERROR: [youtube] abc: Private video. Sign in if you've been granted access\n",
        );
        assert!(f.permanent);
        assert_eq!(
            f.message,
            "Private video. Sign in if you've been granted access"
        );
        let f = classify(
            "ERROR: [youtube] abc: Join this channel to get access to members-only content",
        );
        assert!(f.permanent);
        let f = classify("ERROR: unable to download video data: HTTP Error 429: Too Many Requests");
        assert!(!f.permanent);
        assert_eq!(
            f.message,
            "unable to download video data: HTTP Error 429: Too Many Requests"
        );
        let f = classify("ERROR: [youtube] abc: Sign in to confirm you're not a bot");
        assert!(
            !f.permanent,
            "bot checks are retried (and become a problem later)"
        );
        assert!(!classify("").permanent);
    }
}
