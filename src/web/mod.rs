//! The web UI served by `tidepool run --daemon` (the `server` service): HTML
//! pages (askama templates, htmx) and a JSON API under `/api/v1`.
//!
//! Security: the `Host` header must name the listen address when it's a
//! loopback one (no DNS rebinding); with `server.token` set, every request
//! needs it (a bearer header, or the cookie the sign-in page sets); and every
//! POST must be same-origin: pages send the CSRF token htmx adds, the API
//! takes a bearer token or a JSON body (which browsers can't send cross-site
//! without asking first).

pub mod data;
mod versions;

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use askama::Template;
use axum::Router;
use axum::extract::{Form, Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Json, Redirect, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use tokio::sync::Notify;

use crate::config::ServerConfig;
use crate::config::store::ConfigStore;
use crate::daemon::service::Service;
use crate::state::State as Db;
use crate::state::daemon as store;

const SESSION_COOKIE: &str = "tidepool_session";

/// What every request handler shares.
pub struct Web {
    db: PathBuf,
    token: String,
    csrf: String,
    /// `Host` values accepted; empty: any (a token protects the server).
    hosts: Vec<String>,
    sync_now: Arc<Notify>,
    cleanup_now: Arc<Notify>,
    /// Wakes the processor (it does the work asked on video pages).
    processor: Arc<Notify>,
    /// Where hook logs are (`<video_id>.log`).
    hook_logs: PathBuf,
    /// tidepool's data folder (the MusicBrainz cache).
    data_dir: PathBuf,
    /// `server.read_only_config`: no editing the config.
    read_only: bool,
    /// Ends the live event streams when the server stops.
    shutdown: crate::shutdown::Shutdown,
    /// What yt-dlp and Deno said their versions are.
    versions: Arc<versions::Versions>,
}

/// The daemon's signals the web UI uses.
pub struct Wake {
    pub sync_now: Arc<Notify>,
    pub cleanup_now: Arc<Notify>,
    pub processor: Arc<Notify>,
    /// The server's: stopping it ends the live event streams.
    pub shutdown: crate::shutdown::Shutdown,
}

impl Web {
    #[must_use]
    pub fn new(
        db: PathBuf,
        data_dir: PathBuf,
        hook_logs: PathBuf,
        server: &ServerConfig,
        addr: SocketAddr,
        wake: Wake,
    ) -> Self {
        let port = addr.port();
        let hosts = if addr.ip().is_loopback() {
            vec![
                format!("127.0.0.1:{port}"),
                format!("localhost:{port}"),
                format!("[::1]:{port}"),
            ]
        } else {
            Vec::new()
        };
        Self {
            db,
            token: crate::config::secret::resolve(&server.token).unwrap_or_default(),
            csrf: random_hex(16),
            hosts,
            sync_now: wake.sync_now,
            cleanup_now: wake.cleanup_now,
            processor: wake.processor,
            hook_logs,
            data_dir,
            read_only: server.read_only_config,
            shutdown: wake.shutdown,
            versions: Arc::default(),
        }
    }

    /// What the sign-in cookie holds: derived from the token, so changing
    /// the token signs everyone out.
    fn session(&self) -> String {
        use sha2::Digest;
        let digest = sha2::Sha256::digest(format!("tidepool-session:{}", self.token));
        crate::hex::encode(&digest)
    }

    /// Run `f` on a database connection, off the async threads.
    async fn db<T: Send + 'static>(
        self: &Arc<Self>,
        f: impl FnOnce(&rusqlite::Connection) -> anyhow::Result<T> + Send + 'static,
    ) -> Result<T, WebError> {
        let path = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let state = Db::open(&path)?;
            f(&state.conn)
        })
        .await
        .map_err(|e| WebError(anyhow::anyhow!(e)))?
        .map_err(WebError)
    }

    /// tidepool's version, and the yt-dlp and Deno the config uses.
    async fn versions(self: &Arc<Self>) -> Result<Vec<versions::Software>, WebError> {
        let tools = self
            .db(|conn| Ok(ConfigStore::new(conn).load_current()?.1.config.tools))
            .await?;
        let (versions, bin_dir) = (self.versions.clone(), self.data_dir.join("bin"));
        tokio::task::spawn_blocking(move || versions.all(&tools, &bin_dir))
            .await
            .map_err(|e| WebError(anyhow::anyhow!(e)))
    }

    async fn status(self: &Arc<Self>) -> Result<data::Status, WebError> {
        self.db(|conn| {
            let (_, loaded) = ConfigStore::new(conn).load_current()?;
            Ok(data::status(conn, &loaded.config)?)
        })
        .await
    }
}

/// Serve `web` on `listener` until `shutdown` stops it.
///
/// # Errors
///
/// Fails if accepting connections fails.
pub async fn serve(
    web: Arc<Web>,
    listener: tokio::net::TcpListener,
    shutdown: crate::shutdown::Shutdown,
) -> std::io::Result<()> {
    axum::serve(listener, router(web))
        .with_graceful_shutdown(async move { shutdown.stopping().await })
        .await
}

pub fn router(web: Arc<Web>) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/fragments/status", get(status_fragment))
        .route("/fragments/live", get(live_fragment))
        .route("/api/v1/events", get(api_events))
        .route("/actions/sync", post(action_sync))
        .route("/actions/cleanup", post(action_cleanup))
        .route("/actions/services/{name}/{action}", post(action_service))
        .route("/actions/downloads/{state}", post(action_downloads))
        .route(
            "/actions/sources/{name}/downloads/{state}",
            post(action_source_downloads),
        )
        .route("/sources/{name}", get(source_page))
        .route("/sources/{name}/videos/{id}", get(video_page))
        .route("/sources/{name}/videos/{id}/jobs", get(jobs_fragment))
        .route("/sources/{name}/videos/{id}/{action}", post(video_action))
        .route("/config", get(config_page))
        .route("/config/check", post(config_check))
        .route("/config/save", post(config_save))
        .route("/config/revisions/{rev}", get(revision_page))
        .route("/config/rollback/{rev}", post(config_rollback))
        .route("/login", get(login_page).post(login))
        .route("/assets/{file}", get(asset))
        .route("/api/v1/status", get(api_status))
        .route("/api/v1/sync", post(api_sync))
        .route("/api/v1/cleanup", post(api_cleanup))
        .route("/api/v1/services/{name}/{action}", post(api_service))
        .layer(middleware::from_fn_with_state(web.clone(), guard))
        .layer(middleware::map_response(no_store))
        .with_state(web)
}

/// An error shown as a plain 500.
#[derive(Debug)]
pub struct WebError(anyhow::Error);

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        tracing::error!("web UI: {:#}", self.0);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("something went wrong: {:#}", self.0),
        )
            .into_response()
    }
}

fn render(template: impl Template) -> Result<Html<String>, WebError> {
    template
        .render()
        .map(Html)
        .map_err(|e| WebError(anyhow::anyhow!(e)))
}

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardPage {
    csrf: String,
    status: data::Status,
    message: Option<String>,
    read_only: bool,
    live: crate::events::Live,
    versions: Vec<versions::Software>,
}

#[derive(Template)]
#[template(path = "live.html")]
struct LiveFragment {
    live: crate::events::Live,
}

async fn live_fragment() -> Result<Html<String>, WebError> {
    render(LiveFragment {
        live: crate::events::live(),
    })
}

/// What happens as it happens, as server-sent events (JSON data, one
/// `Event` each). Ends when the server stops.
async fn api_events(State(web): State<Arc<Web>>) -> Response {
    use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
    use tokio::sync::broadcast::error::RecvError;

    let Some(receiver) = crate::events::subscribe() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no live events here").into_response();
    };
    let shutdown = web.shutdown.clone();
    let stream = futures_util::stream::unfold(
        (receiver, shutdown),
        |(mut receiver, shutdown)| async move {
            let data = tokio::select! {
                () = shutdown.stopping() => return None,
                got = receiver.recv() => match got {
                    Ok(event) => serde_json::to_string(&event).expect("events serialize"),
                    // Too far behind: say something changed, the page reloads.
                    Err(RecvError::Lagged(_)) => r#"{"type":"changed"}"#.to_string(),
                    Err(RecvError::Closed) => return None,
                },
            };
            Some((
                Ok::<_, std::convert::Infallible>(SseEvent::default().data(data)),
                (receiver, shutdown),
            ))
        },
    );
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

#[derive(Template)]
#[template(path = "status.html")]
struct StatusFragment {
    status: data::Status,
    message: Option<String>,
    read_only: bool,
}

#[derive(Template)]
#[template(path = "login.html")]
struct LoginPage {
    csrf: String,
    failed: bool,
}

async fn dashboard(State(web): State<Arc<Web>>) -> Result<Html<String>, WebError> {
    let (status, versions) = tokio::try_join!(web.status(), web.versions())?;
    render(DashboardPage {
        csrf: web.csrf.clone(),
        status,
        message: None,
        read_only: web.read_only,
        live: crate::events::live(),
        versions,
    })
}

async fn status_fragment(State(web): State<Arc<Web>>) -> Result<Html<String>, WebError> {
    fragment(&web, None).await
}

async fn fragment(web: &Arc<Web>, message: Option<String>) -> Result<Html<String>, WebError> {
    let status = web.status().await?;
    render(StatusFragment {
        status,
        message,
        read_only: web.read_only,
    })
}

/// Sync now; returns what happened.
async fn sync_now(web: &Arc<Web>) -> Result<String, WebError> {
    let paused = web
        .db(|conn| Ok(store::service(conn, Service::Syncer.name())?.paused))
        .await?;
    if paused {
        return Ok("The syncer is paused; resume it first.".into());
    }
    web.sync_now.notify_one();
    Ok("Syncing now.".into())
}

async fn cleanup_now(web: &Arc<Web>) -> Result<String, WebError> {
    let paused = web
        .db(|conn| Ok(store::service(conn, Service::Syncer.name())?.paused))
        .await?;
    if paused {
        return Ok("The syncer is paused; resume it first.".into());
    }
    web.cleanup_now.notify_one();
    Ok("Cleaning up now.".into())
}

/// Pause or resume a service; `None` when there's no such service or
/// action.
async fn set_service(web: &Arc<Web>, name: &str, action: &str) -> Result<Option<String>, WebError> {
    let (Some(service), Some(pause)) = (
        Service::parse(name),
        match action {
            "pause" => Some(true),
            "resume" => Some(false),
            _ => None,
        },
    ) else {
        return Ok(None);
    };
    web.db(move |conn| Ok(store::set_paused(conn, service.name(), pause)?))
        .await?;
    Ok(Some(if pause {
        format!("Pausing the {name}.")
    } else {
        format!("Resuming the {name}.")
    }))
}

async fn action_sync(State(web): State<Arc<Web>>) -> Result<Html<String>, WebError> {
    let message = sync_now(&web).await?;
    fragment(&web, Some(message)).await
}

async fn action_cleanup(State(web): State<Arc<Web>>) -> Result<Html<String>, WebError> {
    let message = cleanup_now(&web).await?;
    fragment(&web, Some(message)).await
}

async fn action_service(
    State(web): State<Arc<Web>>,
    Path((name, action)): Path<(String, String)>,
) -> Result<Response, WebError> {
    match set_service(&web, &name, &action).await? {
        Some(message) => Ok(fragment(&web, Some(message)).await?.into_response()),
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

/// Turn downloads on or off (`source`: one source's `download`, else
/// `general.download`), as a new config revision written to the config file
/// too. Returns what to say.
async fn set_downloads(
    web: &Arc<Web>,
    source: Option<String>,
    state: &str,
) -> Result<Option<String>, WebError> {
    let on = match state {
        "on" => true,
        "off" => false,
        _ => return Ok(None),
    };
    if web.read_only {
        return Ok(Some(
            "Editing the config is off (server.read_only_config).".into(),
        ));
    }
    web.db(move |conn| {
        let current = ConfigStore::new(conn)
            .current()?
            .ok_or_else(|| anyhow::anyhow!("there's no stored config yet"))?;
        let mut doc: toml_edit::DocumentMut = current.toml.parse()?;
        let what = if let Some(key) = &source {
            let Some(table) = doc
                .get_mut("sources")
                .and_then(|s| s.get_mut(key.as_str()))
                .and_then(|t| t.as_table_like_mut())
            else {
                return Ok(None);
            };
            table.insert("download", toml_edit::value(on));
            format!("for {key}")
        } else {
            if doc.get("general").is_none() {
                doc["general"] = toml_edit::table();
            }
            doc["general"]["download"] = toml_edit::value(on);
            "for every source without its own setting".to_string()
        };
        let text = doc.to_string();
        let loaded = crate::config::load_str("config", &text)?;
        let (rev, written) = store_and_write(conn, |store| Ok(store.save_edit(&text, &loaded)?))?;
        let mut message = format!(
            "Downloads are {} {what} (config revision {rev}).",
            if on { "on" } else { "off" }
        );
        if let Err(problem) = written {
            let _ = write!(message, " But {problem}.");
        }
        Ok(Some(message))
    })
    .await
}

async fn action_downloads(
    State(web): State<Arc<Web>>,
    Path(state): Path<String>,
) -> Result<Response, WebError> {
    match set_downloads(&web, None, &state).await? {
        Some(message) => Ok(fragment(&web, Some(message)).await?.into_response()),
        None => Ok(not_found("such state")),
    }
}

async fn action_source_downloads(
    State(web): State<Arc<Web>>,
    Path((name, state)): Path<(String, String)>,
) -> Result<Response, WebError> {
    match set_downloads(&web, Some(name), &state).await? {
        Some(message) => Ok(fragment(&web, Some(message)).await?.into_response()),
        None => Ok(not_found("such source or state")),
    }
}

async fn api_status(State(web): State<Arc<Web>>) -> Result<Json<data::Status>, WebError> {
    Ok(Json(web.status().await?))
}

fn api_message(message: &str) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "message": message }))
}

async fn api_sync(State(web): State<Arc<Web>>) -> Result<Json<serde_json::Value>, WebError> {
    Ok(api_message(&sync_now(&web).await?))
}

async fn api_cleanup(State(web): State<Arc<Web>>) -> Result<Json<serde_json::Value>, WebError> {
    Ok(api_message(&cleanup_now(&web).await?))
}

async fn api_service(
    State(web): State<Arc<Web>>,
    Path((name, action)): Path<(String, String)>,
) -> Result<Response, WebError> {
    match set_service(&web, &name, &action).await? {
        Some(message) => Ok(api_message(&message).into_response()),
        None => Ok((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "no such service or action" })),
        )
            .into_response()),
    }
}

const PER_PAGE: usize = 50;

/// The artist's name `clean_title = "music_video"` needs for `source`'s
/// videos, when it's on and the source names the artist; `None` otherwise
/// (or when MusicBrainz can't say: titles then show as they are).
async fn clean_title_artist(
    web: &Arc<Web>,
    source: &crate::config::merge::EffectiveSource,
) -> Option<String> {
    if source.clean_title != crate::config::CleanTitle::MusicVideo {
        return None;
    }
    let id =
        crate::download::stages::artist_id(source, &crate::download::layout::Values::default())?;
    crate::musicbrainz::artist_name(&source.embed.musicbrainz_server, &web.data_dir, &id)
        .await
        .map_err(|e| tracing::debug!("web UI: no artist name for {}: {e}", source.key))
        .ok()
}

/// A source from the current config.
async fn source_named(
    web: &Arc<Web>,
    name: &str,
) -> Result<Option<crate::config::merge::EffectiveSource>, WebError> {
    let name = name.to_string();
    web.db(move |conn| {
        let (_, loaded) = ConfigStore::new(conn).load_current()?;
        Ok(crate::config::merge::effective_sources(&loaded.config)
            .into_iter()
            .find(|s| s.key == name))
    })
    .await
}

#[derive(Deserialize, Default)]
struct ListQuery {
    status: Option<String>,
    q: Option<String>,
    page: Option<usize>,
}

struct GroupOption {
    key: &'static str,
    label: &'static str,
    count: i64,
    selected: bool,
}

#[derive(Template)]
#[template(path = "source.html")]
struct SourcePage {
    csrf: String,
    name: String,
    title: Option<String>,
    status: String,
    query: String,
    groups: Vec<GroupOption>,
    all_count: i64,
    rows: Vec<data::VideoRow>,
    page: usize,
    pages: usize,
    total: usize,
}

impl SourcePage {
    /// The list filtered by `status` group (all of them when empty),
    /// keeping the search.
    fn status_link(&self, status: &str) -> String {
        format!(
            "/sources/{}?status={}&q={}",
            encode(&self.name),
            encode(status),
            encode(&self.query)
        )
    }

    fn page_link(&self, page: usize) -> String {
        format!(
            "/sources/{}?status={}&q={}&page={page}",
            encode(&self.name),
            encode(&self.status),
            encode(&self.query)
        )
    }
}

/// Percent-encode for a URL path segment or query value.
fn encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn not_found(what: &str) -> Response {
    (StatusCode::NOT_FOUND, format!("no {what}")).into_response()
}

async fn source_page(
    State(web): State<Arc<Web>>,
    Path(name): Path<String>,
    axum::extract::Query(query): axum::extract::Query<ListQuery>,
) -> Result<Response, WebError> {
    let status = query
        .status
        .filter(|s| data::GROUPS.iter().any(|(key, _)| key == s))
        .unwrap_or_default();
    let text = query.q.unwrap_or_default();
    let page = query.page.unwrap_or(1).max(1);
    let csrf = web.csrf.clone();
    let found = web
        .db(move |conn| {
            let (_, loaded) = ConfigStore::new(conn).load_current()?;
            if !crate::config::merge::effective_sources(&loaded.config)
                .iter()
                .any(|s| s.key == name)
            {
                return Ok(None);
            }
            let title: Option<String> = conn
                .query_row("SELECT title FROM sources WHERE key = ?1", [&name], |r| {
                    r.get(0)
                })
                .unwrap_or(None);
            let mut counts: std::collections::HashMap<&'static str, i64> =
                std::collections::HashMap::default();
            let mut stmt = conn.prepare(
                "SELECT status, COUNT(*) FROM items WHERE source_key = ?1 GROUP BY status",
            )?;
            for row in stmt.query_map([&name], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })? {
                let (s, n) = row?;
                *counts.entry(data::group(&s)).or_default() += n;
            }
            let (rows, total) = data::videos(
                conn,
                &name,
                (!status.is_empty()).then_some(status.as_str()),
                Some(text.as_str()),
                page,
                PER_PAGE,
            )?;
            Ok(Some(SourcePage {
                csrf,
                groups: data::GROUPS
                    .iter()
                    .map(|(key, label)| GroupOption {
                        key,
                        label,
                        count: counts.get(key).copied().unwrap_or(0),
                        selected: status == *key,
                    })
                    .collect(),
                all_count: counts.values().sum(),
                title,
                rows,
                page,
                pages: total.div_ceil(PER_PAGE).max(1),
                total,
                status,
                query: text,
                name,
            }))
        })
        .await?;
    let Some(mut page) = found else {
        return Ok(not_found("such source"));
    };
    if let Some(source) = source_named(&web, &page.name).await?
        && let Some(artist) = clean_title_artist(&web, &source).await
    {
        for row in &mut page.rows {
            row.clean_title = data::clean_title(&row.title, &artist);
        }
    }
    Ok(render(page)?.into_response())
}

#[derive(Template)]
#[template(path = "video.html")]
struct VideoPage {
    csrf: String,
    video: data::VideoDetail,
    jobs: Vec<store::Job>,
    busy: bool,
    message: Option<String>,
    log: Option<String>,
}

#[derive(Template)]
#[template(path = "jobs.html")]
struct JobsFragment {
    video: data::VideoDetail,
    jobs: Vec<store::Job>,
    busy: bool,
    message: Option<String>,
}

/// A video and its latest jobs, if `source` has it.
async fn video_and_jobs(
    web: &Arc<Web>,
    source: String,
    id: String,
) -> Result<Option<(data::VideoDetail, Vec<store::Job>)>, WebError> {
    web.db(move |conn| {
        let (_, loaded) = ConfigStore::new(conn).load_current()?;
        let Some(video) = data::video(conn, &loaded.config, &source, &id)? else {
            return Ok(None);
        };
        let jobs = store::jobs_for(conn, &source, &id, 10)?;
        Ok(Some((video, jobs)))
    })
    .await
}

async fn video_page(
    State(web): State<Arc<Web>>,
    Path((source, id)): Path<(String, String)>,
) -> Result<Response, WebError> {
    let Some((mut video, jobs)) = video_and_jobs(&web, source.clone(), id.clone()).await? else {
        return Ok(not_found("such video in this source"));
    };
    if let Some(source) = source_named(&web, &source).await?
        && let Some(artist) = clean_title_artist(&web, &source).await
    {
        video.clean_title = data::clean_title(&video.title, &artist);
    }
    let log = data::log_tail(&web.hook_logs.join(format!("{id}.log")), 40);
    Ok(render(VideoPage {
        csrf: web.csrf.clone(),
        busy: jobs.iter().any(|j| j.done_at.is_none()),
        video,
        jobs,
        message: None,
        log,
    })?
    .into_response())
}

async fn jobs_fragment(
    State(web): State<Arc<Web>>,
    Path((source, id)): Path<(String, String)>,
) -> Result<Response, WebError> {
    jobs_response(&web, source, id, None).await
}

async fn jobs_response(
    web: &Arc<Web>,
    source: String,
    id: String,
    message: Option<String>,
) -> Result<Response, WebError> {
    let Some((video, jobs)) = video_and_jobs(web, source, id).await? else {
        return Ok(not_found("such video in this source"));
    };
    Ok(render(JobsFragment {
        busy: jobs.iter().any(|j| j.done_at.is_none()),
        video,
        jobs,
        message,
    })?
    .into_response())
}

/// Queue work on a video for the processor.
async fn video_action(
    State(web): State<Arc<Web>>,
    Path((source, id, action)): Path<(String, String, String)>,
) -> Result<Response, WebError> {
    if !store::JOB_ACTIONS.contains(&action.as_str()) {
        return Ok(not_found("such action"));
    }
    let (s, v, a) = (source.clone(), id.clone(), action.clone());
    let queued = web
        .db(move |conn| {
            let known: i64 = conn.query_row(
                "SELECT COUNT(*) FROM items WHERE source_key = ?1 AND video_id = ?2",
                [&s, &v],
                |r| r.get(0),
            )?;
            if known == 0 {
                return Ok(None);
            }
            store::add_job(conn, &a, &s, &v, jiff::Timestamp::now())?;
            Ok(Some(
                store::service(conn, Service::Processor.name())?.paused,
            ))
        })
        .await?;
    let Some(paused) = queued else {
        return Ok(not_found("such video in this source"));
    };
    web.processor.notify_one();
    let what = match action.as_str() {
        "hooks" => "Running its hooks again",
        "embed" => "Embedding again",
        "reset" => "Putting it back in the download queue",
        "reset_delete" => "Deleting its files and putting it back in the download queue",
        _ => "Restoring it from the trash",
    };
    let message = if paused {
        format!("{what} once the processor is resumed.")
    } else {
        format!("{what}.")
    };
    jobs_response(&web, source, id, Some(message)).await
}

struct RevisionRow {
    rev: i64,
    created_at: String,
    from: String,
}

/// Where a revision came from, in words.
fn origin(revision: &crate::config::store::Revision) -> String {
    use crate::config::store::Source;
    match (revision.source, &revision.source_path) {
        (Source::Import, Some(path)) => format!("imported from {path}"),
        (Source::Ui, _) => "edited in the web UI".into(),
        (Source::Rollback, Some(from)) => {
            format!("rolled back to {}", from.trim_start_matches("rev:"))
        }
        (source, _) => source.as_str().to_string(),
    }
}

#[derive(Template)]
#[template(path = "config.html")]
struct ConfigPage {
    csrf: String,
    rev: i64,
    file: Option<String>,
    text: String,
    read_only: bool,
    history: Vec<RevisionRow>,
    message: Option<String>,
}

#[derive(Template)]
#[template(path = "config_result.html")]
struct ConfigResult {
    errors: Option<String>,
    changes: Vec<String>,
    /// The text differs (settings may not: comments, formatting, the same
    /// key written another way).
    text_changed: bool,
    diff: String,
    rev: i64,
    saved: bool,
}

#[derive(Template)]
#[template(path = "revision.html")]
struct RevisionPage {
    csrf: String,
    rev: i64,
    current: i64,
    created_at: String,
    from: String,
    text: String,
    changes: Vec<String>,
    read_only: bool,
}

#[derive(Deserialize)]
struct ConfigForm {
    text: String,
    base: i64,
}

#[derive(Deserialize, Default)]
struct ConfigQuery {
    saved: Option<i64>,
}

/// The file the config was last imported from: saves are written there too.
fn config_file(store: &ConfigStore<'_>) -> anyhow::Result<Option<PathBuf>> {
    Ok(store
        .last_import()?
        .and_then(|r| r.source_path)
        .map(PathBuf::from))
}

async fn config_page(
    State(web): State<Arc<Web>>,
    axum::extract::Query(query): axum::extract::Query<ConfigQuery>,
) -> Result<Html<String>, WebError> {
    let (csrf, read_only) = (web.csrf.clone(), web.read_only);
    let page = web
        .db(move |conn| {
            let store = ConfigStore::new(conn);
            let current = store.current()?;
            let file = config_file(&store)?;
            let history = store
                .history()?
                .iter()
                .rev()
                .map(|r| RevisionRow {
                    rev: r.rev,
                    created_at: r.created_at.clone(),
                    from: origin(r),
                })
                .collect();
            let message = query.saved.map(|rev| match &file {
                Some(file) => format!("Saved as revision {rev} and written to {}.", file.display()),
                None => format!("Saved as revision {rev}."),
            });
            Ok(ConfigPage {
                csrf,
                rev: current.as_ref().map_or(0, |c| c.rev),
                text: current
                    .as_ref()
                    .map(|c| crate::config::diff::mask_toml(&c.toml))
                    .unwrap_or_default(),
                file: file.map(|f| f.display().to_string()),
                read_only,
                history,
                message,
            })
        })
        .await?;
    render(page)
}

/// Check edited text against the current revision: its mistakes, or what
/// it changes. Also returns the text with its secrets restored.
fn compare(
    conn: &rusqlite::Connection,
    edited: &str,
) -> anyhow::Result<(ConfigResult, Option<(String, crate::config::Loaded)>)> {
    let current = ConfigStore::new(conn).current()?;
    let (rev, toml, json) = match &current {
        Some(c) => (c.rev, c.toml.clone(), c.json.clone()),
        None => (0, String::new(), "{}".to_string()),
    };
    let text = crate::config::diff::unmask_toml(edited, &toml);
    match crate::config::load_str("config", &text) {
        Err(err) => Ok((
            ConfigResult {
                errors: Some(err.to_string()),
                changes: Vec::new(),
                text_changed: false,
                diff: String::new(),
                rev,
                saved: false,
            },
            None,
        )),
        Ok(loaded) => {
            let changes = crate::app::config_changes(&json, loaded.json.clone())?;
            let diff = crate::config::diff::text_diff(
                &crate::config::diff::mask_toml(&toml),
                &crate::config::diff::mask_toml(&text),
                &format!("revision {rev}"),
                "edited",
            );
            Ok((
                ConfigResult {
                    errors: None,
                    changes,
                    text_changed: text != toml,
                    diff,
                    rev,
                    saved: false,
                },
                Some((text, loaded)),
            ))
        }
    }
}

async fn config_check(
    State(web): State<Arc<Web>>,
    Form(form): Form<ConfigForm>,
) -> Result<Html<String>, WebError> {
    let result = web.db(move |conn| Ok(compare(conn, &form.text)?.0)).await?;
    render(result)
}

fn read_only() -> Response {
    (
        StatusCode::FORBIDDEN,
        "editing the config is off (server.read_only_config)",
    )
        .into_response()
}

/// Back to the config page, saying what was saved.
fn saved(rev: i64, headers: &HeaderMap) -> Response {
    let to = format!("/config?saved={rev}");
    if headers.contains_key("hx-request") {
        ([("hx-redirect", to)], StatusCode::OK).into_response()
    } else {
        Redirect::to(&to).into_response()
    }
}

/// Save a new revision and write it to the config file; `Err` text says
/// what went wrong with the file (the revision is saved anyway).
fn store_and_write(
    conn: &rusqlite::Connection,
    save: impl FnOnce(&ConfigStore<'_>) -> anyhow::Result<i64>,
) -> anyhow::Result<(i64, Result<(), String>)> {
    let store = ConfigStore::new(conn);
    let rev = save(&store)?;
    let text = store.get(rev)?.toml;
    let written = match config_file(&store)? {
        Some(path) => crate::config::store::write_file(&path, &text)
            .map(drop)
            .map_err(|e| format!("writing {} failed: {e}", path.display())),
        None => Ok(()),
    };
    tracing::info!("config revision {rev} saved in the web UI");
    Ok((rev, written))
}

async fn config_save(
    State(web): State<Arc<Web>>,
    headers: HeaderMap,
    Form(form): Form<ConfigForm>,
) -> Result<Response, WebError> {
    enum Outcome {
        Result(ConfigResult),
        Saved(i64, Result<(), String>),
    }
    if web.read_only {
        return Ok(read_only());
    }
    let outcome = web
        .db(move |conn| {
            let (mut result, checked) = compare(conn, &form.text)?;
            if result.rev != form.base {
                result.errors = Some(format!(
                    "The config changed since you opened this page (it's revision {} now). \
                     Reload to see it; saving would have replaced it.",
                    result.rev
                ));
                return Ok(Outcome::Result(result));
            }
            let Some((text, loaded)) = checked.filter(|_| result.text_changed) else {
                return Ok(Outcome::Result(result));
            };
            let (rev, written) =
                store_and_write(conn, |store| Ok(store.save_edit(&text, &loaded)?))?;
            Ok(Outcome::Saved(rev, written))
        })
        .await?;
    Ok(match outcome {
        Outcome::Result(result) => render(result)?.into_response(),
        Outcome::Saved(rev, Ok(())) => saved(rev, &headers),
        Outcome::Saved(rev, Err(problem)) => render(ConfigResult {
            errors: Some(format!("Saved as revision {rev}, but {problem}.")),
            changes: Vec::new(),
            text_changed: false,
            diff: String::new(),
            rev,
            saved: true,
        })?
        .into_response(),
    })
}

async fn revision_page(
    State(web): State<Arc<Web>>,
    Path(rev): Path<i64>,
) -> Result<Response, WebError> {
    let (csrf, read_only) = (web.csrf.clone(), web.read_only);
    let page = web
        .db(move |conn| {
            let store = ConfigStore::new(conn);
            let Ok(revision) = store.get(rev) else {
                return Ok(None);
            };
            let current = store.current()?.expect("a revision exists");
            let changes =
                crate::app::config_changes(&current.json, serde_json::from_str(&revision.json)?)?;
            Ok(Some(RevisionPage {
                csrf,
                rev,
                current: current.rev,
                created_at: revision.created_at.clone(),
                from: origin(&revision),
                text: crate::config::diff::mask_toml(&revision.toml),
                changes,
                read_only,
            }))
        })
        .await?;
    match page {
        Some(page) => Ok(render(page)?.into_response()),
        None => Ok(not_found("such revision")),
    }
}

async fn config_rollback(
    State(web): State<Arc<Web>>,
    headers: HeaderMap,
    Path(rev): Path<i64>,
) -> Result<Response, WebError> {
    if web.read_only {
        return Ok(read_only());
    }
    let (new, written) = web
        .db(move |conn| store_and_write(conn, |store| Ok(store.rollback(rev)?)))
        .await?;
    if let Err(problem) = written {
        tracing::warn!("config revision {new}: {problem}");
    }
    Ok(saved(new, &headers))
}

async fn login_page(State(web): State<Arc<Web>>) -> Result<Html<String>, WebError> {
    render(LoginPage {
        csrf: web.csrf.clone(),
        failed: false,
    })
}

#[derive(Deserialize)]
struct LoginForm {
    token: String,
}

async fn login(
    State(web): State<Arc<Web>>,
    Form(form): Form<LoginForm>,
) -> Result<Response, WebError> {
    if web.token.is_empty() {
        return Ok(Redirect::to("/").into_response());
    }
    if !same(&form.token, &web.token) {
        let page = render(LoginPage {
            csrf: web.csrf.clone(),
            failed: true,
        })?;
        return Ok((StatusCode::UNAUTHORIZED, page).into_response());
    }
    let cookie = format!(
        "{SESSION_COOKIE}={}; Path=/; HttpOnly; SameSite=Strict",
        web.session()
    );
    let mut response = Redirect::to("/").into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).expect("a valid cookie"),
    );
    Ok(response)
}

/// An asset built into the binary, and its content type.
fn asset_file(file: &str) -> Option<(&'static str, &'static str)> {
    Some(match file {
        "htmx.min.js" => (
            include_str!("../../assets/vendor/htmx.min.js"),
            "text/javascript",
        ),
        "app.css" => (include_str!("../../assets/app.css"), "text/css"),
        "live.js" => (include_str!("../../assets/live.js"), "text/javascript"),
        "toml.js" => (include_str!("../../assets/toml.js"), "text/javascript"),
        _ => return None,
    })
}

/// A short tag of an asset's contents.
fn asset_tag(body: &str) -> String {
    let digest = <sha2::Sha256 as sha2::Digest>::digest(body.as_bytes());
    crate::hex::encode(&digest)[..16].to_string()
}

/// An asset's address, which changes with its contents, so browsers never
/// use a copy from an older build.
pub(crate) fn asset_url(file: &str) -> String {
    let tag = asset_file(file)
        .map(|(body, _)| asset_tag(body))
        .unwrap_or_default();
    format!("/assets/{file}?v={tag}")
}

/// An asset built into the binary. The browser checks back each time (a
/// tag of its contents), so a new build's assets show up right away.
async fn asset(Path(file): Path<String>, headers: HeaderMap) -> Response {
    let Some((body, kind)) = asset_file(&file) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let etag = format!("\"{}\"", asset_tag(body));
    let cache = [
        (header::ETAG, etag.as_str()),
        (header::CACHE_CONTROL, "no-cache"),
    ];
    let fresh = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(',').any(|tag| tag.trim() == etag));
    if fresh {
        return (StatusCode::NOT_MODIFIED, cache).into_response();
    }
    (cache, [(header::CONTENT_TYPE, kind)], body).into_response()
}

/// Pages, fragments and API answers are always fetched fresh, so a browser
/// never shows a page from an older build (or older state). Assets say
/// their own.
async fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .entry(header::CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-store"));
    response
}

/// Host, origin, sign-in and CSRF checks, before any handler.
async fn guard(State(web): State<Arc<Web>>, request: Request, next: Next) -> Response {
    let headers = request.headers();
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default()
        .to_string();
    if !web.hosts.is_empty() && !web.hosts.iter().any(|h| h.eq_ignore_ascii_case(&host)) {
        return (StatusCode::MISDIRECTED_REQUEST, "unknown host").into_response();
    }
    let path = request.uri().path().to_string();
    let api = path.starts_with("/api/");
    let post = request.method() == Method::POST;
    // A POST from another site's page.
    if post
        && let Some(origin) = headers.get(header::ORIGIN).and_then(|o| o.to_str().ok())
        && origin.split("://").nth(1) != Some(host.as_str())
    {
        return (StatusCode::FORBIDDEN, "cross-site request").into_response();
    }
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|a| a.to_str().ok())
        .and_then(|a| a.strip_prefix("Bearer "))
        .is_some_and(|t| !web.token.is_empty() && same(t, &web.token));
    let open = path == "/login" || path.starts_with("/assets/");
    if !web.token.is_empty() && !open && !bearer && !signed_in(&web, headers) {
        return if api {
            (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({ "error": "sign in with a bearer token" })),
            )
                .into_response()
        } else if headers.contains_key("hx-request") {
            ([("hx-redirect", "/login")], StatusCode::UNAUTHORIZED).into_response()
        } else {
            Redirect::to("/login").into_response()
        };
    }
    if post && path != "/login" && !bearer {
        let csrf_ok = headers
            .get("x-csrf-token")
            .and_then(|t| t.to_str().ok())
            .is_some_and(|t| same(t, &web.csrf));
        let json = headers
            .get(header::CONTENT_TYPE)
            .and_then(|c| c.to_str().ok())
            .is_some_and(|c| c.starts_with("application/json"));
        if !(csrf_ok || (api && json)) {
            return (
                StatusCode::FORBIDDEN,
                if api {
                    "send Content-Type: application/json (or a bearer token)"
                } else {
                    "missing CSRF token; reload the page"
                },
            )
                .into_response();
        }
    }
    next.run(request).await
}

fn signed_in(web: &Web, headers: &HeaderMap) -> bool {
    let session = web.session();
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|c| c.to_str().ok())
        .flat_map(|c| c.split(';'))
        .filter_map(|c| c.trim().split_once('='))
        .any(|(name, value)| name == SESSION_COOKIE && same(value, &session))
}

/// Compare secrets in constant time.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |diff, (x, y)| diff | (x ^ y))
            == 0
}

fn random_hex(bytes: usize) -> String {
    use std::io::Read;
    let mut buf = vec![0u8; bytes];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .expect("/dev/urandom");
    crate::hex::encode(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    const CONFIG: &str = "# my config\n[server]\ntoken = \"my-long-secret-token\"\n\n\
                          [sources.lofi]\nurls = ['https://www.youtube.com/playlist?list=PLaaaaaaaa']\n";

    struct Setup {
        dir: tempfile::TempDir,
        web: Arc<Web>,
        db: PathBuf,
    }

    fn setup(token: &str) -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        let state = Db::open(&db).unwrap();
        let file = dir.path().join("config.toml");
        std::fs::write(&file, CONFIG).unwrap();
        let name = file.display().to_string();
        let loaded = crate::config::load_str(&name, CONFIG).unwrap();
        ConfigStore::new(&state.conn)
            .import(Some(&name), CONFIG, &loaded)
            .unwrap();
        let server = ServerConfig {
            token: token.into(),
            ..ServerConfig::default()
        };
        let web = Arc::new(Web::new(
            db.clone(),
            dir.path().join("data"),
            dir.path().join("logs"),
            &server,
            "127.0.0.1:8787".parse().unwrap(),
            Wake {
                sync_now: Arc::new(Notify::new()),
                cleanup_now: Arc::new(Notify::new()),
                processor: Arc::new(Notify::new()),
                shutdown: crate::shutdown::Shutdown::default(),
            },
        ));
        Setup { dir, web, db }
    }

    async fn send(
        web: &Arc<Web>,
        request: axum::http::request::Builder,
    ) -> (StatusCode, HeaderMap, String) {
        let request = request
            .header_if_missing(header::HOST, "127.0.0.1:8787")
            .body(Body::empty())
            .unwrap();
        let response = router(web.clone()).oneshot(request).await.unwrap();
        let (parts, body) = response.into_parts();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        (
            parts.status,
            parts.headers,
            String::from_utf8_lossy(&bytes).into_owned(),
        )
    }

    trait HeaderIfMissing {
        fn header_if_missing(self, name: header::HeaderName, value: &str) -> Self;
    }

    impl HeaderIfMissing for axum::http::request::Builder {
        fn header_if_missing(self, name: header::HeaderName, value: &str) -> Self {
            if self.headers_ref().is_some_and(|h| h.contains_key(&name)) {
                self
            } else {
                self.header(name, value)
            }
        }
    }

    fn get(uri: &str) -> axum::http::request::Builder {
        Request::builder().uri(uri)
    }

    fn post(uri: &str) -> axum::http::request::Builder {
        Request::builder().method(Method::POST).uri(uri)
    }

    /// Videos in `lofi`: a downloaded one, one an exclude rule dropped, a
    /// failed one, and 60 waiting.
    fn seed(db: &std::path::Path) {
        let state = Db::open(db).unwrap();
        let conn = &state.conn;
        conn.execute(
            "INSERT INTO sources (key, lists, title) VALUES ('lofi', '[]', 'Lofi Girl')",
            [],
        )
        .unwrap();
        let files = serde_json::json!({"dir": "/m/lofi/a", "media": "/m/lofi/a/a.mkv", "metadata": "/m/lofi/a/metadata.json"});
        let dropped = crate::filter::Decision::Excluded { rule: 0 }.to_json();
        let add = |id: String,
                   title: &str,
                   position: i64,
                   status: &str,
                   extra: (&str, Option<String>)| {
            conn.execute(
                "INSERT INTO videos (id, title, published_at) VALUES (?1, ?2, '2026-09-01T00:00:00Z')",
                rusqlite::params![id, title],
            )
            .unwrap();
            conn.execute(
                &format!(
                    "INSERT INTO items (source_key, video_id, position, in_remote, status, {})
                     VALUES ('lofi', ?1, ?2, 1, ?3, ?4)",
                    extra.0
                ),
                rusqlite::params![id, position, status, extra.1],
            )
            .unwrap();
        };
        add(
            "aaaaaaaaaaa".into(),
            "Rainy beats",
            1,
            "done",
            ("files", Some(files.to_string())),
        );
        add(
            "bbbbbbbbbbb".into(),
            "Lofi trailer",
            2,
            "filtered",
            ("decision", Some(dropped)),
        );
        add(
            "ccccccccccc".into(),
            "Broken upload",
            3,
            "failed",
            ("last_error", Some("HTTP 429".into())),
        );
        for n in 0..60 {
            add(
                format!("w{n:010}"),
                &format!("Waiting {n}"),
                10 + n,
                "pending",
                ("last_error", None),
            );
        }
    }

    #[tokio::test]
    async fn a_source_lists_its_videos_with_filters_and_pages() {
        let s = setup("");
        seed(&s.db);
        let (status, _, body) = send(&s.web, get("/sources/lofi")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("Lofi Girl") && body.contains("Rainy beats"));
        assert!(body.contains("Page 1 of 2 · 63 videos"), "{body}");
        assert!(body.contains("Skipped <b>1</b>") && body.contains("Waiting <b>60</b>"));
        assert!(
            body.contains("/sources/lofi?status=&#38;q=&#38;page=2"),
            "{body}"
        );
        assert!(!body.contains("Waiting 59"), "on page 2");

        let (_, _, body) = send(&s.web, get("/sources/lofi?status=skipped")).await;
        assert!(body.contains("Lofi trailer") && !body.contains("Rainy beats"));
        assert!(body.contains("drop (matches exclude[0])"), "why: {body}");
        assert!(
            body.contains(r#"<span class="pill st-excluded">excluded</span>"#),
            "{body}"
        );
        assert!(!body.contains(">filtered<"));
        let (_, _, body) = send(&s.web, get("/sources/lofi?status=failed")).await;
        assert!(body.contains("HTTP 429"));
        let (_, _, body) = send(&s.web, get("/sources/lofi?q=RAINY")).await;
        assert!(body.contains("Rainy beats") && !body.contains("Broken upload"));
        assert!(body.contains("1 videos") || body.contains("Page 1 of 1"));
        let (_, _, body) = send(&s.web, get("/sources/lofi?q=100%25")).await;
        assert!(body.contains("No videos match"), "a % is searched as is");

        let (status, _, _) = send(&s.web, get("/sources/nope")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn lists_show_the_title_clean_title_will_use() {
        use wiremock::matchers::path;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let musicbrainz = MockServer::start().await;
        Mock::given(path("/ws/2/artist/lofi-girl"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "lofi-girl", "name": "Lofi Girl"})),
            )
            .expect(1)
            .mount(&musicbrainz)
            .await;
        let s = setup("");
        seed(&s.db);
        let state = Db::open(&s.db).unwrap();
        state
            .conn
            .execute(
                "UPDATE videos SET title = 'Lofi Girl - Night Drive (Official Video)' WHERE id = 'aaaaaaaaaaa'",
                [],
            )
            .unwrap();
        let text = format!(
            "{CONFIG}clean_title = \"music_video\"\n\
             embed.tags = {{ MUSICBRAINZ_ARTISTID = \"lofi-girl\" }}\n\
             embed.musicbrainz = {{ server = \"{}\" }}\n",
            musicbrainz.uri()
        );
        let loaded = crate::config::load_str("c.toml", &text).unwrap();
        ConfigStore::new(&state.conn)
            .save_edit(&text, &loaded)
            .unwrap();

        let (_, _, body) = send(&s.web, get("/sources/lofi")).await;
        assert!(body.contains(">Night Drive</a>"), "{body}");
        assert!(
            body.contains("Lofi Girl - Night Drive (Official Video)"),
            "YouTube's too"
        );
        assert!(body.contains(">Lofi trailer</a>"), "nothing to clean");
        let (_, _, body) = send(&s.web, get("/sources/lofi/videos/aaaaaaaaaaa")).await;
        assert!(body.contains("<h1>Night Drive</h1>"), "{body}");
    }

    #[tokio::test]
    async fn a_video_page_explains_it_and_queues_work() {
        let s = setup("");
        seed(&s.db);
        let (status, _, body) = send(&s.web, get("/sources/lofi/videos/aaaaaaaaaaa")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            body.contains("Rainy beats") && body.contains("source lofi (done)"),
            "{body}"
        );
        assert!(body.contains("/m/lofi/a/a.mkv"));
        assert!(
            body.contains("Run hooks again") && body.contains("Delete files and download again")
        );
        assert!(!body.contains("Restore from trash"), "not in the trash");

        let (status, _, body) = send(
            &s.web,
            post("/sources/lofi/videos/aaaaaaaaaaa/hooks")
                .header("x-csrf-token", s.web.csrf.as_str()),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains("Running its hooks again.") && body.contains("waiting"),
            "{body}"
        );
        assert!(body.contains("every 2s"), "refreshes until it's done");
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            s.web.processor.notified(),
        )
        .await
        .expect("the processor was woken");
        let state = Db::open(&s.db).unwrap();
        let jobs = store::waiting_jobs(&state.conn).unwrap();
        assert_eq!((jobs.len(), jobs[0].action.as_str()), (1, "hooks"));

        let (status, _, _) = send(
            &s.web,
            post("/sources/lofi/videos/aaaaaaaaaaa/explode")
                .header("x-csrf-token", s.web.csrf.as_str()),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _, _) = send(&s.web, get("/sources/lofi/videos/zzzzzzzzzzz")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    fn form(uri: &str, csrf: &str, body: String) -> Request<Body> {
        Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(header::HOST, "127.0.0.1:8787")
            .header("x-csrf-token", csrf)
            .header("hx-request", "true")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap()
    }

    fn form_body(text: &str, base: i64) -> String {
        format!("base={base}&text={}", encode(text))
    }

    async fn call(web: &Arc<Web>, request: Request<Body>) -> (StatusCode, HeaderMap, String) {
        let response = router(web.clone()).oneshot(request).await.unwrap();
        let (parts, body) = response.into_parts();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        (
            parts.status,
            parts.headers,
            String::from_utf8_lossy(&bytes).into_owned(),
        )
    }

    #[tokio::test]
    async fn the_config_editor_checks_saves_and_keeps_secrets() {
        let s = setup("");
        let (status, _, page) = send(&s.web, get("/config")).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert!(
            page.contains("my-l…oken") && !page.contains("my-long-secret-token"),
            "{page}"
        );
        assert!(page.contains("Revision 1") && page.contains("imported from"));
        let shown = crate::config::diff::mask_toml(CONFIG);

        // Mistakes are shown, nothing is saved.
        let broken = shown.replace("urls =", "urlz =");
        let (_, _, body) = call(
            &s.web,
            form("/config/check", &s.web.csrf, form_body(&broken, 1)),
        )
        .await;
        assert!(
            body.contains("It has mistakes") && body.contains("urlz"),
            "{body}"
        );

        // A change: listed, then saved with the real token, and written to
        // the file (the old one kept as .bak).
        let edited = shown.replace("PLaaaaaaaa", "PLbbbbbbbb");
        let (_, _, body) = call(
            &s.web,
            form("/config/check", &s.web.csrf, form_body(&edited, 1)),
        )
        .await;
        assert!(body.contains("sources.lofi.urls"), "{body}");
        let (status, headers, _) = call(
            &s.web,
            form("/config/save", &s.web.csrf, form_body(&edited, 1)),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["hx-redirect"], "/config?saved=2");
        let state = Db::open(&s.db).unwrap();
        let current = ConfigStore::new(&state.conn).current().unwrap().unwrap();
        assert_eq!(current.rev, 2);
        assert!(
            current.toml.contains("my-long-secret-token") && current.toml.contains("PLbbbbbbbb")
        );
        let file = s.dir.path().join("config.toml");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), current.toml);
        assert_eq!(
            std::fs::read_to_string(s.dir.path().join("config.toml.bak")).unwrap(),
            CONFIG
        );
        let (_, _, page) = send(&s.web, get("/config?saved=2")).await;
        assert!(
            page.contains("Saved as revision 2 and written to"),
            "{page}"
        );

        // The same settings written another way: kept as a new revision.
        let reworded = edited.replace(
            "[sources.lofi]\nurls",
            "# the lofi one\n[sources.lofi]\nurls",
        );
        let (_, _, body) = call(
            &s.web,
            form("/config/check", &s.web.csrf, form_body(&reworded, 2)),
        )
        .await;
        assert!(
            body.contains("Only comments or formatting changed"),
            "{body}"
        );
        let (_, headers, _) = call(
            &s.web,
            form("/config/save", &s.web.csrf, form_body(&reworded, 2)),
        )
        .await;
        assert_eq!(headers["hx-redirect"], "/config?saved=3");
        let current = ConfigStore::new(&state.conn).current().unwrap().unwrap();
        assert!(current.toml.contains("# the lofi one"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), current.toml);
        let (_, _, body) = call(
            &s.web,
            form("/config/save", &s.web.csrf, form_body(&reworded, 3)),
        )
        .await;
        assert!(body.contains("No changes from revision 3"), "{body}");

        // Saving from a page opened before that would replace it: refused.
        let (_, _, body) = call(
            &s.web,
            form("/config/save", &s.web.csrf, form_body(&shown, 1)),
        )
        .await;
        assert!(
            body.contains("changed since you opened this page"),
            "{body}"
        );

        // Roll back to 1: a new revision 3, in the file too.
        let (_, _, page) = send(&s.web, get("/config/revisions/1")).await;
        assert!(
            page.contains("Roll back to revision 1") && page.contains("PLaaaaaaaa"),
            "{page}"
        );
        let (status, headers, _) = call(
            &s.web,
            form("/config/rollback/1", &s.web.csrf, String::new()),
        )
        .await;
        assert_eq!(
            (status, &headers["hx-redirect"]),
            (StatusCode::OK, &HeaderValue::from_static("/config?saved=4"))
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), CONFIG);
    }

    #[tokio::test]
    async fn a_read_only_config_cant_be_saved() {
        let s = setup("");
        let web = Arc::new(Web {
            read_only: true,
            ..Web::new(
                s.db.clone(),
                s.dir.path().join("data"),
                s.dir.path().join("logs"),
                &ServerConfig::default(),
                "127.0.0.1:8787".parse().unwrap(),
                Wake {
                    sync_now: Arc::new(Notify::new()),
                    cleanup_now: Arc::new(Notify::new()),
                    processor: Arc::new(Notify::new()),
                    shutdown: crate::shutdown::Shutdown::default(),
                },
            )
        });
        let (_, _, page) = send(&web, get("/config")).await;
        assert!(page.contains("Editing is off") && page.contains("readonly"));
        let shown = crate::config::diff::mask_toml(CONFIG).replace("PLaaaaaaaa", "PLbbbbbbbb");
        let (status, _, _) =
            call(&web, form("/config/save", &web.csrf, form_body(&shown, 1))).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _, _) = call(&web, form("/config/rollback/1", &web.csrf, String::new())).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn the_live_panel_and_events_follow_the_work() {
        use futures_util::StreamExt;
        crate::events::enable();
        let s = setup("");
        crate::events::emit(crate::events::Event::Downloading {
            source: "lofi".into(),
            video_id: "livetest001".into(),
            title: "Live test".into(),
            percent: Some(42),
            size: Some("10 MB".into()),
            speed: None,
            eta: None,
        });
        let (_, _, body) = send(&s.web, get("/fragments/live")).await;
        assert!(
            body.contains("Live test") && body.contains(">Downloading<") && body.contains(">42%<"),
            "{body}"
        );
        let (_, _, page) = send(&s.web, get("/")).await;
        assert!(page.contains("In progress") && page.contains("/assets/live.js"));
        assert!(!page.contains("every 5s"), "events replace polling");

        let request = get("/api/v1/events")
            .header(header::HOST, "127.0.0.1:8787")
            .body(Body::empty())
            .unwrap();
        let response = router(s.web.clone()).oneshot(request).await.unwrap();
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/event-stream"
        );
        let mut stream = response.into_body().into_data_stream();
        crate::events::emit(crate::events::Event::Saved {
            source: "lofi".into(),
            video_id: "livetest001".into(),
            title: "Live test".into(),
        });
        let mut seen = String::new();
        while !seen.contains("livetest001") {
            let chunk = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
                .await
                .expect("an event")
                .unwrap()
                .unwrap();
            seen.push_str(&String::from_utf8_lossy(&chunk));
        }
        assert!(seen.contains(r#""type":"saved""#), "{seen}");
        let (_, _, body) = send(&s.web, get("/fragments/live")).await;
        assert!(
            body.contains("Just now") && body.contains("saved"),
            "{body}"
        );

        // Stopping the server ends the stream.
        s.web.shutdown.stop(143);
        let end = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while stream.next().await.is_some() {}
        })
        .await;
        assert!(end.is_ok(), "the stream ended");
    }

    #[tokio::test]
    async fn downloads_are_turned_on_and_off_from_the_dashboard() {
        let s = setup("");
        let (_, _, page) = send(&s.web, get("/")).await;
        assert!(
            page.contains("Downloads are <strong>on</strong> by default"),
            "{page}"
        );
        let toggle = |uri: &str| post(uri).header("x-csrf-token", s.web.csrf.as_str());

        let (status, _, body) = send(&s.web, toggle("/actions/sources/lofi/downloads/off")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains("Downloads are off for lofi (config revision 2)."),
            "{body}"
        );
        assert!(body.contains("st-off\">Downloads off"), "{body}");
        let file = std::fs::read_to_string(s.dir.path().join("config.toml")).unwrap();
        assert!(file.starts_with("# my config"), "comments kept");
        assert!(file.contains("download = false"), "{file}");

        let (_, _, body) = send(&s.web, toggle("/actions/downloads/off")).await;
        assert!(
            body.contains("Downloads are off for every source without its own setting"),
            "{body}"
        );
        let state = Db::open(&s.db).unwrap();
        let (_, loaded) = ConfigStore::new(&state.conn).load_current().unwrap();
        assert!(!loaded.config.general.download);
        assert_eq!(loaded.config.sources["lofi"].download, Some(false));
        let (_, _, body) = send(&s.web, toggle("/actions/sources/lofi/downloads/on")).await;
        assert!(body.contains("Downloads are on for lofi"));

        let (status, _, _) = send(&s.web, toggle("/actions/sources/nope/downloads/off")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _, _) = send(&s.web, toggle("/actions/downloads/maybe")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_dashboard_shows_sources_and_services() {
        let s = setup("");
        let (status, _, body) = send(&s.web, get("/")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("lofi"), "{body}");
        assert!(body.contains("Sync now") && body.contains("processor"));
        assert!(body.contains(&s.web.csrf), "htmx sends the CSRF token");
        let (status, _, body) = send(&s.web, get("/assets/htmx.min.js")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.starts_with("var htmx"));
        let (_, _, page) = send(&s.web, get("/")).await;
        assert!(page.contains(&asset_url("app.css")), "{page}");
        assert!(asset_url("app.css").len() > "/assets/app.css?v=".len());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_dashboard_shows_what_runs() {
        use std::os::unix::fs::PermissionsExt;

        let s = setup("");
        let (_, _, body) = send(&s.web, get("/")).await;
        let tidepool = concat!("tidepool ", env!("CARGO_PKG_VERSION"));
        assert!(body.contains(tidepool), "{body}");
        assert!(
            body.contains("yt-dlp <em>not downloaded yet</em>"),
            "{body}"
        );
        assert!(
            body.contains("YouTube API: no key, so yt-dlp indexes"),
            "{body}"
        );

        let bin = s.dir.path().join("data/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let channel = crate::config::types::YtdlpConfig::default().channel;
        let ytdlp = bin.join(crate::tools::manager::Tool::Ytdlp(channel).file_name());
        std::fs::write(&ytdlp, "#!/bin/sh\necho 2026.10.01\n").unwrap();
        std::fs::set_permissions(&ytdlp, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (_, _, body) = send(&s.web, get("/")).await;
        assert!(body.contains(">yt-dlp 2026.10.01<"), "{body}");
    }

    #[tokio::test]
    async fn pages_and_assets_are_never_used_stale() {
        let s = setup("");
        let (status, headers, _) = send(&s.web, get("/assets/app.css")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CACHE_CONTROL], "no-cache");
        for page in ["/", "/login"] {
            let (_, page_headers, _) = send(&s.web, get(page)).await;
            assert_eq!(page_headers[header::CACHE_CONTROL], "no-store", "{page}");
        }
        let etag = headers[header::ETAG].to_str().unwrap().to_string();
        let (status, _, body) = send(
            &s.web,
            get("/assets/app.css").header(header::IF_NONE_MATCH, &etag),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_MODIFIED);
        assert!(body.is_empty());
        let (status, _, _) = send(
            &s.web,
            get("/assets/app.css").header(header::IF_NONE_MATCH, "\"old\""),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn only_its_own_address_and_origin_are_accepted() {
        let s = setup("");
        let (status, _, _) = send(&s.web, get("/").header(header::HOST, "evil.example:8787")).await;
        assert_eq!(status, StatusCode::MISDIRECTED_REQUEST, "DNS rebinding");
        let (status, _, _) = send(&s.web, get("/").header(header::HOST, "localhost:8787")).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _, _) = send(
            &s.web,
            post("/actions/sync")
                .header("x-csrf-token", s.web.csrf.as_str())
                .header(header::ORIGIN, "http://evil.example"),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn actions_need_the_csrf_token() {
        let s = setup("");
        let (status, _, _) = send(&s.web, post("/actions/sync")).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _, body) = send(
            &s.web,
            post("/actions/sync").header("x-csrf-token", s.web.csrf.as_str()),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Syncing now."), "{body}");
        tokio::time::timeout(std::time::Duration::from_secs(1), s.web.sync_now.notified())
            .await
            .expect("the syncer was told");

        let (status, _, body) = send(
            &s.web,
            post("/actions/services/processor/pause").header("x-csrf-token", s.web.csrf.as_str()),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Pausing the processor."));
        let state = Db::open(&s.db).unwrap();
        assert!(store::service(&state.conn, "processor").unwrap().paused);
        let (status, _, _) = send(
            &s.web,
            post("/actions/services/nope/pause").header("x-csrf-token", s.web.csrf.as_str()),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_api_takes_json_posts() {
        let s = setup("");
        let (status, _, body) = send(&s.web, get("/api/v1/status")).await;
        assert_eq!(status, StatusCode::OK);
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["sources"][0]["name"], "lofi");
        let (status, _, _) = send(&s.web, post("/api/v1/cleanup")).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "a plain form post could be cross-site"
        );
        let (status, _, body) = send(
            &s.web,
            post("/api/v1/cleanup").header(header::CONTENT_TYPE, "application/json"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Cleaning up now."));
    }

    #[tokio::test]
    async fn a_token_is_needed_when_set() {
        let s = setup("s3cret");
        let (status, headers, _) = send(&s.web, get("/")).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(headers[header::LOCATION], "/login");
        let (status, _, _) = send(&s.web, get("/api/v1/status")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _, _) = send(
            &s.web,
            get("/api/v1/status").header(header::AUTHORIZATION, "Bearer s3cret"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _, _) = send(
            &s.web,
            post("/api/v1/sync").header(header::AUTHORIZATION, "Bearer s3cret"),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "a bearer token is enough for the API"
        );

        let login = |token: &str| {
            Request::builder()
                .method(Method::POST)
                .uri("/login")
                .header(header::HOST, "127.0.0.1:8787")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(format!("token={token}")))
                .unwrap()
        };
        let response = router(s.web.clone()).oneshot(login("nope")).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let response = router(s.web.clone())
            .oneshot(login("s3cret"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let cookie = response.headers()[header::SET_COOKIE].to_str().unwrap();
        assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"));
        assert!(!cookie.contains("s3cret"), "the cookie isn't the token");
        let session = cookie.split(';').next().unwrap();
        let (status, _, body) = send(&s.web, get("/").header(header::COOKIE, session)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
}
