//! Hooks: the user's own commands, run at each stage of a download.
//!
//! - `downloaded`: on the raw files in the work area, before embedding. stdin
//!   gets the files as JSON; stdout may return replacements (e.g. after
//!   transcoding).
//! - `embed`: stdin gets the data about to be embedded; stdout returns it
//!   processed.
//! - `finished`: after the files are in place; stdout is ignored.
//!
//! Hooks of one type run in the order the source lists them, each getting
//! the previous one's output. Empty stdout means "no change".

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use jiff::Timestamp;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::config::placeholders::{self, PLACEHOLDERS, Setting};
use crate::config::{HookConfig, HookType, OnFailure};
use crate::download::info::Chapter;

/// The most stdout a hook may print.
const MAX_OUTPUT: u64 = 16 * 1024 * 1024;
/// How long a hook gets between SIGTERM and SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(10);

/// Everything one hook run needs.
pub struct Run<'a> {
    pub name: &'a str,
    pub hook: &'a HookConfig,
    /// Placeholder values (unsanitized), also exported as `TIDEPOOL_*`.
    pub values: &'a BTreeMap<String, String>,
    /// Where it runs unless the hook sets `work_dir`.
    pub default_work_dir: &'a Path,
    pub home: &'a Path,
    pub input: &'a [u8],
    /// stderr is appended here.
    pub log: &'a Path,
    pub cancel: &'a CancellationToken,
}

/// How a hook (with its retries) went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub name: String,
    pub kind: HookType,
    /// stdout on success, the reason on failure.
    pub result: Result<Vec<u8>, String>,
    pub exit_code: Option<i32>,
    pub attempts: u32,
    pub started_at: Timestamp,
    pub finished_at: Timestamp,
    /// The placeholder values it ran with, to run it again later.
    pub values: BTreeMap<String, String>,
}

impl Report {
    pub fn ok(&self) -> bool {
        self.result.is_ok()
    }
}

/// Run a hook, retrying per `on_failure = "retry"`.
pub async fn run(run: &Run<'_>) -> Report {
    let started_at = Timestamp::now();
    let tries = match run.hook.on_failure {
        OnFailure::Retry => run.hook.retries + 1,
        _ => 1,
    };
    let mut attempts = 0;
    let (result, exit_code) = loop {
        attempts += 1;
        let (result, code) = run_once(run).await;
        if result.is_ok() || attempts >= tries || run.cancel.is_cancelled() {
            break (result, code);
        }
        tracing::debug!(
            "hook {} failed (attempt {attempts} of {tries}): {}",
            run.name,
            result.as_ref().err().map(String::as_str).unwrap_or("")
        );
    };
    Report {
        name: run.name.to_string(),
        kind: run.hook.kind,
        result,
        exit_code,
        attempts,
        started_at,
        finished_at: Timestamp::now(),
        values: run.values.clone(),
    }
}

/// The argv with placeholders filled in; the program's `~` and `$VARS` expanded.
pub fn argv(
    hook: &HookConfig,
    values: &BTreeMap<String, String>,
    home: &Path,
) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for (i, element) in hook.command.iter().enumerate() {
        let segments = placeholders::validate(element, Setting::Hook)
            .map_err(|e| format!("{element:?}: {e}"))?;
        let rendered =
            placeholders::render(&segments, Setting::Hook, |name| values.get(name).cloned());
        if i == 0 && (rendered.starts_with('~') || rendered.starts_with('$')) {
            out.push(crate::paths::expand(&rendered, home)?.display().to_string());
        } else {
            out.push(rendered);
        }
    }
    Ok(out)
}

/// `TIDEPOOL_*` variables for the placeholder values.
pub fn env(kind: HookType, values: &BTreeMap<String, String>) -> Vec<(String, String)> {
    let mut out = vec![("TIDEPOOL_HOOK_TYPE".to_string(), kind.as_str().to_string())];
    for p in PLACEHOLDERS {
        if let (Some(var), Some(value)) = (p.env, values.get(p.name)) {
            out.push((var.to_string(), value.clone()));
        }
    }
    out
}

async fn run_once(run: &Run<'_>) -> (Result<Vec<u8>, String>, Option<i32>) {
    let argv = match argv(run.hook, run.values, run.home) {
        Ok(argv) => argv,
        Err(e) => return (Err(e), None),
    };
    let work_dir = match &run.hook.work_dir {
        Some(dir) => match crate::paths::expand(dir, run.home) {
            Ok(dir) => dir,
            Err(e) => return (Err(format!("work_dir: {e}")), None),
        },
        None => run.default_work_dir.to_path_buf(),
    };
    let log = match open_log(run.log, run.name, run.hook.kind) {
        Ok(log) => log,
        Err(e) => return (Err(format!("cannot open {}: {e}", run.log.display())), None),
    };
    let mut command = tokio::process::Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .current_dir(&work_dir)
        .envs(env(run.hook.kind, run.values))
        .envs(&run.hook.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(log))
        .process_group(0)
        .kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return (Err(format!("cannot run {}: {e}", argv[0])), None),
    };

    let mut stdin = child.stdin.take().expect("piped stdin");
    let input = run.input.to_vec();
    let writer = tokio::spawn(async move {
        // A hook that doesn't read its input is fine.
        let _ = stdin.write_all(&input).await;
    });
    let stdout = child.stdout.take().expect("piped stdout");
    let reader = tokio::spawn(async move {
        let mut buf = Vec::new();
        let mut limited = stdout.take(MAX_OUTPUT + 1);
        limited.read_to_end(&mut buf).await.map(|_| buf)
    });

    let timeout = run.hook.timeout.0;
    let status = tokio::select! {
        status = child.wait() => status,
        _ = tokio::time::sleep(timeout) => {
            stop(&mut child).await;
            return (Err(format!("timed out after {}", crate::config::types::format_duration(timeout))), None);
        }
        _ = run.cancel.cancelled() => {
            stop(&mut child).await;
            return (Err("cancelled".into()), None);
        }
    };
    let _ = writer.await;
    let output = reader.await.ok().and_then(Result::ok).unwrap_or_default();
    let status = match status {
        Ok(status) => status,
        Err(e) => return (Err(e.to_string()), None),
    };
    if !status.success() {
        let detail = last_log_line(run.log)
            .map(|l| format!(": {l}"))
            .unwrap_or_default();
        return (Err(format!("exited with {status}{detail}")), status.code());
    }
    if output.len() as u64 > MAX_OUTPUT {
        return (Err("printed more than 16 MiB".into()), status.code());
    }
    (Ok(output), status.code())
}

/// SIGTERM to the hook's process group, SIGKILL after a grace period.
async fn stop(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        // SAFETY: signalling the process group of a child we spawned.
        unsafe {
            libc::kill(-(pid as libc::pid_t), libc::SIGTERM);
        }
        if tokio::time::timeout(KILL_GRACE, child.wait()).await.is_ok() {
            return;
        }
        // SAFETY: as above.
        unsafe {
            libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
        }
    }
    let _ = child.kill().await;
}

fn open_log(path: &Path, name: &str, kind: HookType) -> std::io::Result<std::fs::File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(
        file,
        "== {} hook {name} ({}) ==",
        Timestamp::now(),
        kind.as_str()
    )?;
    Ok(file)
}

/// The last thing the hook printed to stderr, for the failure message.
fn last_log_line(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    text.lines()
        .rev()
        .take_while(|l| !l.starts_with("== "))
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
}

/// A hook's stdout as JSON; empty means "no change".
fn parse_output(name: &str, output: &[u8]) -> Result<Option<Value>, String> {
    if output.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    let value: Value = serde_json::from_slice(output)
        .map_err(|e| format!("hook {name} printed invalid JSON: {e}"))?;
    if !value.is_object() {
        return Err(format!("hook {name} must print a JSON object"));
    }
    Ok(Some(value))
}

/// A path a hook returned: a string naming an existing file.
fn existing_file(name: &str, what: &str, value: &Value) -> Result<PathBuf, String> {
    let path = value
        .as_str()
        .map(PathBuf::from)
        .ok_or_else(|| format!("hook {name}: {what} must be a path string"))?;
    if !path.is_absolute() || !path.is_file() {
        return Err(format!(
            "hook {name}: {what} {} isn't an existing file (use a full path)",
            path.display()
        ));
    }
    Ok(path)
}

/// A subtitle file and its language.
#[derive(Debug, Clone, PartialEq)]
pub struct Subtitle {
    pub lang: String,
    pub path: PathBuf,
}

fn subtitles_json(subtitles: &[Subtitle]) -> Value {
    subtitles
        .iter()
        .map(|s| json!({ "lang": s.lang, "path": s.path }))
        .collect()
}

fn parse_subtitles(name: &str, value: &Value) -> Result<Vec<Subtitle>, String> {
    if value.is_null() {
        return Ok(Vec::new());
    }
    let items = value
        .as_array()
        .ok_or_else(|| format!("hook {name}: subtitles must be a list"))?;
    items
        .iter()
        .map(|s| {
            let lang = s["lang"]
                .as_str()
                .filter(|l| !l.is_empty())
                .ok_or_else(|| format!("hook {name}: each subtitle needs a lang"))?;
            Ok(Subtitle {
                lang: lang.to_string(),
                path: existing_file(name, "subtitle path", &s["path"])?,
            })
        })
        .collect()
}

/// The hooks of `kind` a source lists, in its order.
pub fn of_type<'a>(
    names: &'a [String],
    hooks: &'a BTreeMap<String, HookConfig>,
    kind: HookType,
) -> impl Iterator<Item = (&'a str, &'a HookConfig)> {
    names.iter().filter_map(move |name| {
        hooks
            .get(name)
            .filter(|h| h.kind == kind)
            .map(|h| (name.as_str(), h))
    })
}

/// The raw files `downloaded` hooks see and may replace.
#[derive(Debug, Clone, PartialEq)]
pub struct Files {
    pub media: PathBuf,
    pub cover: Option<PathBuf>,
    pub subtitles: Vec<Subtitle>,
}

impl Files {
    pub fn to_json(&self, context: &Value) -> Value {
        json!({
            "files": {
                "media": self.media,
                "cover": self.cover,
                "subtitles": subtitles_json(&self.subtitles),
            },
            "context": context,
        })
    }

    /// Apply a `downloaded` hook's output. Missing keys keep their value;
    /// `null` drops the cover or the subtitles (the media can't be dropped).
    pub fn apply(&mut self, name: &str, output: &[u8]) -> Result<(), String> {
        let Some(value) = parse_output(name, output)? else {
            return Ok(());
        };
        let Some(files) = value.get("files") else {
            return Ok(());
        };
        if let Some(media) = files.get("media") {
            self.media = existing_file(name, "files.media", media)?;
        }
        if let Some(cover) = files.get("cover") {
            self.cover = if cover.is_null() {
                None
            } else {
                Some(existing_file(name, "files.cover", cover)?)
            };
        }
        if let Some(subtitles) = files.get("subtitles") {
            self.subtitles = parse_subtitles(name, subtitles)?;
        }
        Ok(())
    }
}

/// The placed files `finished` hooks see, and may report as moved (a new
/// path) or removed on purpose (`None`).
#[derive(Debug, Clone, PartialEq)]
pub struct Placed {
    pub media: Option<PathBuf>,
    pub cover: Option<PathBuf>,
    pub subtitles: Vec<Subtitle>,
}

impl Placed {
    pub fn to_json(&self, context: &Value) -> Value {
        json!({
            "files": {
                "media": self.media,
                "cover": self.cover,
                "subtitles": subtitles_json(&self.subtitles),
            },
            "context": context,
        })
    }

    /// Apply a `finished` hook's output: a path (anywhere, must exist) says
    /// where a file is now; `null` says it was removed on purpose; a
    /// subtitle list replaces the current one. Missing keys keep their value.
    pub fn apply(&mut self, name: &str, output: &[u8]) -> Result<(), String> {
        let Some(value) = parse_output(name, output)? else {
            return Ok(());
        };
        let Some(files) = value.get("files") else {
            return Ok(());
        };
        for (key, slot) in [("media", &mut self.media), ("cover", &mut self.cover)] {
            if let Some(path) = files.get(key) {
                *slot = if path.is_null() {
                    None
                } else {
                    Some(existing_file(name, &format!("files.{key}"), path)?)
                };
            }
        }
        if let Some(subtitles) = files.get("subtitles") {
            self.subtitles = parse_subtitles(name, subtitles)?;
        }
        Ok(())
    }
}

/// What gets embedded; `embed` hooks may change all of it.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbedData {
    pub tags: BTreeMap<String, String>,
    pub cover: Option<PathBuf>,
    pub chapters: Vec<Chapter>,
    pub subtitles: Vec<Subtitle>,
}

impl EmbedData {
    pub fn to_json(&self, context: &Value) -> Value {
        let chapters: Vec<Value> = self
            .chapters
            .iter()
            .map(|c| json!({ "start": c.start_time, "end": c.end_time, "title": c.title }))
            .collect();
        json!({
            "tags": self.tags,
            "cover": self.cover.as_ref().map(|p| json!({ "path": p })),
            "chapters": chapters,
            "subtitles": subtitles_json(&self.subtitles),
            "context": context,
        })
    }

    /// Apply an `embed` hook's output. Missing keys keep their value; `null`
    /// removes it (a tag, the cover, all chapters or subtitles). `context`
    /// is ignored.
    pub fn apply(&mut self, name: &str, output: &[u8]) -> Result<(), String> {
        let Some(value) = parse_output(name, output)? else {
            return Ok(());
        };
        if let Some(tags) = value.get("tags") {
            match tags {
                Value::Null => self.tags.clear(),
                Value::Object(map) => {
                    for (key, value) in map {
                        match value {
                            Value::Null => {
                                self.tags.remove(key);
                            }
                            Value::String(s) => {
                                self.tags.insert(key.clone(), s.clone());
                            }
                            Value::Number(n) => {
                                self.tags.insert(key.clone(), n.to_string());
                            }
                            _ => {
                                return Err(format!("hook {name}: tag {key:?} must be a string"));
                            }
                        }
                    }
                }
                _ => return Err(format!("hook {name}: tags must be an object")),
            }
        }
        if let Some(cover) = value.get("cover") {
            self.cover = if cover.is_null() {
                None
            } else {
                let path = existing_file(name, "cover.path", &cover["path"])?;
                let ok = path
                    .extension()
                    .and_then(|e| {
                        crate::download::thumbnail::CoverFormat::from_extension(
                            &e.to_string_lossy(),
                        )
                    })
                    .is_some();
                if !ok {
                    return Err(format!(
                        "hook {name}: cover.path must end in .jpg, .jpeg, .png or .webp"
                    ));
                }
                Some(path)
            };
        }
        if let Some(chapters) = value.get("chapters") {
            self.chapters = if chapters.is_null() {
                Vec::new()
            } else {
                let items = chapters
                    .as_array()
                    .ok_or_else(|| format!("hook {name}: chapters must be a list"))?;
                items
                    .iter()
                    .map(|c| {
                        let (Some(start), Some(end)) = (c["start"].as_f64(), c["end"].as_f64())
                        else {
                            return Err(format!(
                                "hook {name}: each chapter needs start and end seconds"
                            ));
                        };
                        if start > end || start < 0.0 {
                            return Err(format!(
                                "hook {name}: chapter {start}–{end} ends before it starts"
                            ));
                        }
                        Ok(Chapter {
                            start_time: start,
                            end_time: end,
                            title: c["title"].as_str().unwrap_or_default().to_string(),
                        })
                    })
                    .collect::<Result<_, _>>()?
            };
        }
        if let Some(subtitles) = value.get("subtitles") {
            self.subtitles = parse_subtitles(name, subtitles)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::HumanDuration;

    fn hook(command: &[&str], kind: HookType) -> HookConfig {
        HookConfig {
            command: command.iter().map(|s| s.to_string()).collect(),
            kind,
            timeout: HumanDuration(Duration::from_secs(10)),
            on_failure: OnFailure::Fail,
            retries: 0,
            env: BTreeMap::new(),
            work_dir: None,
        }
    }

    fn values() -> BTreeMap<String, String> {
        [
            ("id", "dQw4w9WgXcQ"),
            ("title", "AFI - 17 Crimes"),
            ("output_file", "/w/media.mkv"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    }

    async fn go(hook: &HookConfig, input: &str, dir: &Path) -> Report {
        let values = values();
        let cancel = CancellationToken::new();
        run(&Run {
            name: "h",
            hook,
            values: &values,
            default_work_dir: dir,
            home: Path::new("/home/me"),
            input: input.as_bytes(),
            log: &dir.join("logs/x.log"),
            cancel: &cancel,
        })
        .await
    }

    #[test]
    fn renders_argv_and_env() {
        let h = hook(
            &["~/bin/x", "{title}", "--file={output_file}", "{channel}"],
            HookType::Embed,
        );
        assert_eq!(
            argv(&h, &values(), Path::new("/home/me")).unwrap(),
            [
                "/home/me/bin/x",
                "AFI - 17 Crimes",
                "--file=/w/media.mkv",
                ""
            ]
        );
        let env = env(HookType::Embed, &values());
        assert!(env.contains(&("TIDEPOOL_HOOK_TYPE".into(), "embed".into())));
        assert!(env.contains(&("TIDEPOOL_TITLE".into(), "AFI - 17 Crimes".into())));
        assert!(env.contains(&("TIDEPOOL_OUTPUT_FILE".into(), "/w/media.mkv".into())));
    }

    #[tokio::test]
    async fn runs_with_stdin_stdout_env_and_logs() {
        let dir = tempfile::tempdir().unwrap();
        let h = hook(
            &[
                "sh",
                "-c",
                "echo to-log >&2; pwd; echo \"$TIDEPOOL_VIDEO_ID\"; cat",
            ],
            HookType::Embed,
        );
        let report = go(&h, "input", dir.path()).await;
        let out = String::from_utf8(report.result.unwrap()).unwrap();
        let cwd = std::fs::canonicalize(dir.path()).unwrap();
        assert_eq!(out, format!("{}\ndQw4w9WgXcQ\ninput", cwd.display()));
        let log = std::fs::read_to_string(dir.path().join("logs/x.log")).unwrap();
        assert!(log.contains("hook h (embed) ==\nto-log\n"), "{log}");
    }

    #[tokio::test]
    async fn reports_failures_retries_and_timeouts() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = hook(&["sh", "-c", "echo boom >&2; exit 3"], HookType::Finished);
        let report = go(&h, "", dir.path()).await;
        assert_eq!(
            report.result,
            Err("exited with exit status: 3: boom".into())
        );
        assert_eq!((report.exit_code, report.attempts), (Some(3), 1));

        h.on_failure = OnFailure::Retry;
        h.retries = 2;
        let report = go(&h, "", dir.path()).await;
        assert_eq!(report.attempts, 3);

        // Succeeds on the second try: a counter file.
        let counter = dir.path().join("n");
        let script = format!(
            "n=$(cat {c} 2>/dev/null || echo 0); n=$((n+1)); echo $n > {c}; [ $n -ge 2 ]",
            c = counter.display()
        );
        h.command = vec!["sh".into(), "-c".into(), script];
        let report = go(&h, "", dir.path()).await;
        assert!(report.ok());
        assert_eq!(report.attempts, 2);

        let mut slow = hook(&["sleep", "30"], HookType::Finished);
        slow.timeout = HumanDuration(Duration::from_millis(200));
        let started = std::time::Instant::now();
        let report = go(&slow, "", dir.path()).await;
        assert!(report.result.unwrap_err().starts_with("timed out after"));
        assert!(started.elapsed() < Duration::from_secs(5));

        let missing = hook(&["/nonexistent/hook"], HookType::Finished);
        assert!(
            go(&missing, "", dir.path())
                .await
                .result
                .unwrap_err()
                .starts_with("cannot run")
        );
    }

    fn data(dir: &Path) -> EmbedData {
        let cover = dir.join("cover.jpg");
        std::fs::write(&cover, "x").unwrap();
        EmbedData {
            tags: [
                ("title", "AFI - 17 Crimes"),
                ("artist", "AFI AFIREINSIDE"),
                ("genre", "Music"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
            cover: Some(cover),
            chapters: vec![Chapter {
                start_time: 0.0,
                end_time: 1.0,
                title: "Intro".into(),
            }],
            subtitles: vec![],
        }
    }

    #[test]
    fn applies_embed_output() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = data(dir.path());
        let before = d.clone();
        d.apply("h", b"  \n").unwrap();
        assert_eq!(d, before, "empty output: no change");

        d.apply(
            "h",
            br#"{"tags": {"title": "17 Crimes", "artist": "AFI", "genre": null, "track": 3}, "context": {"x": 1}}"#,
        )
        .unwrap();
        assert_eq!(d.tags["title"], "17 Crimes");
        assert_eq!(d.tags["artist"], "AFI");
        assert_eq!(d.tags["track"], "3");
        assert!(!d.tags.contains_key("genre"), "null removes a tag");
        assert_eq!(d.cover, before.cover, "missing keys keep their value");
        assert_eq!(d.chapters, before.chapters);

        d.apply("h", br#"{"cover": null, "chapters": null}"#)
            .unwrap();
        assert_eq!((d.cover.clone(), d.chapters.len()), (None, 0));

        // The full input printed back is a no-op.
        let mut d = data(dir.path());
        let echo = serde_json::to_vec(&d.to_json(&json!({"video": {}}))).unwrap();
        let before = d.clone();
        d.apply("h", &echo).unwrap();
        assert_eq!(d, before);
    }

    #[test]
    fn rejects_bad_embed_output() {
        let dir = tempfile::tempdir().unwrap();
        let gif = dir.path().join("c.gif");
        std::fs::write(&gif, "x").unwrap();
        for (output, error) in [
            ("nope", "invalid JSON"),
            ("[1]", "must print a JSON object"),
            (r#"{"tags": {"a": [1]}}"#, "tag \"a\" must be a string"),
            (r#"{"tags": "x"}"#, "tags must be an object"),
            (
                r#"{"cover": {"path": "/nonexistent.jpg"}}"#,
                "isn't an existing file",
            ),
            (
                &format!(r#"{{"cover": {{"path": "{}"}}}}"#, gif.display()),
                "must end in .jpg",
            ),
            (
                r#"{"chapters": [{"start": 5, "end": 1}]}"#,
                "ends before it starts",
            ),
            (r#"{"chapters": [{"title": "x"}]}"#, "needs start and end"),
            (r#"{"subtitles": [{"path": "/x.vtt"}]}"#, "needs a lang"),
        ] {
            let err = data(dir.path()).apply("h", output.as_bytes()).unwrap_err();
            assert!(err.contains(error), "{output}: {err}");
        }
    }

    #[test]
    fn applies_downloaded_output() {
        let dir = tempfile::tempdir().unwrap();
        let mkv = dir.path().join("media.mkv");
        let mp4 = dir.path().join("media.mp4");
        std::fs::write(&mkv, "a").unwrap();
        std::fs::write(&mp4, "b").unwrap();
        let mut files = Files {
            media: mkv,
            cover: None,
            subtitles: vec![],
        };
        files
            .apply(
                "h",
                format!(r#"{{"files": {{"media": "{}"}}}}"#, mp4.display()).as_bytes(),
            )
            .unwrap();
        assert_eq!(files.media, mp4);
        assert!(
            files
                .apply("h", br#"{"files": {"media": "/gone.mp4"}}"#)
                .is_err()
        );
    }
}
