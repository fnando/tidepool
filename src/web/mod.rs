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
}

impl Web {
    pub fn new(
        db: PathBuf,
        server: &ServerConfig,
        addr: SocketAddr,
        sync_now: Arc<Notify>,
        cleanup_now: Arc<Notify>,
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
            token: server.token.clone(),
            csrf: random_hex(16),
            hosts,
            sync_now,
            cleanup_now,
        }
    }

    /// What the sign-in cookie holds: derived from the token, so changing
    /// the token signs everyone out.
    fn session(&self) -> String {
        use sha2::Digest;
        let digest = sha2::Sha256::digest(format!("tidepool-session:{}", self.token));
        digest.iter().map(|b| format!("{b:02x}")).collect()
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

    async fn status(self: &Arc<Self>) -> Result<data::Status, WebError> {
        self.db(|conn| {
            let (_, loaded) = ConfigStore::new(conn).load_current()?;
            Ok(data::status(conn, &loaded.config)?)
        })
        .await
    }
}

/// Serve `web` on `listener` until `shutdown` stops it.
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
        .route("/actions/sync", post(action_sync))
        .route("/actions/cleanup", post(action_cleanup))
        .route("/actions/services/{name}/{action}", post(action_service))
        .route("/login", get(login_page).post(login))
        .route("/assets/{file}", get(asset))
        .route("/api/v1/status", get(api_status))
        .route("/api/v1/sync", post(api_sync))
        .route("/api/v1/cleanup", post(api_cleanup))
        .route("/api/v1/services/{name}/{action}", post(api_service))
        .layer(middleware::from_fn_with_state(web.clone(), guard))
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
}

#[derive(Template)]
#[template(path = "status.html")]
struct StatusFragment {
    status: data::Status,
    message: Option<String>,
}

#[derive(Template)]
#[template(path = "login.html")]
struct LoginPage {
    csrf: String,
    failed: bool,
}

async fn dashboard(State(web): State<Arc<Web>>) -> Result<Html<String>, WebError> {
    let status = web.status().await?;
    render(DashboardPage {
        csrf: web.csrf.clone(),
        status,
        message: None,
    })
}

async fn status_fragment(State(web): State<Arc<Web>>) -> Result<Html<String>, WebError> {
    fragment(&web, None).await
}

async fn fragment(web: &Arc<Web>, message: Option<String>) -> Result<Html<String>, WebError> {
    let status = web.status().await?;
    render(StatusFragment { status, message })
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

async fn api_status(State(web): State<Arc<Web>>) -> Result<Json<data::Status>, WebError> {
    Ok(Json(web.status().await?))
}

fn api_message(message: String) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "message": message }))
}

async fn api_sync(State(web): State<Arc<Web>>) -> Result<Json<serde_json::Value>, WebError> {
    Ok(api_message(sync_now(&web).await?))
}

async fn api_cleanup(State(web): State<Arc<Web>>) -> Result<Json<serde_json::Value>, WebError> {
    Ok(api_message(cleanup_now(&web).await?))
}

async fn api_service(
    State(web): State<Arc<Web>>,
    Path((name, action)): Path<(String, String)>,
) -> Result<Response, WebError> {
    match set_service(&web, &name, &action).await? {
        Some(message) => Ok(api_message(message).into_response()),
        None => Ok((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "no such service or action" })),
        )
            .into_response()),
    }
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

async fn asset(Path(file): Path<String>) -> Response {
    let (body, kind): (&'static str, &str) = match file.as_str() {
        "pico.min.css" => (include_str!("../../assets/vendor/pico.min.css"), "text/css"),
        "htmx.min.js" => (
            include_str!("../../assets/vendor/htmx.min.js"),
            "text/javascript",
        ),
        "app.css" => (include_str!("../../assets/app.css"), "text/css"),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    (
        [
            (header::CONTENT_TYPE, kind),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        body,
    )
        .into_response()
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
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    struct Setup {
        _dir: tempfile::TempDir,
        web: Arc<Web>,
        db: PathBuf,
    }

    fn setup(token: &str) -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        let state = Db::open(&db).unwrap();
        let text = "[sources.lofi]\nurls = ['https://www.youtube.com/playlist?list=PLaaaaaaaa']\n";
        let loaded = crate::config::load_str("c.toml", text).unwrap();
        ConfigStore::new(&state.conn)
            .import(Some("c.toml"), text, &loaded)
            .unwrap();
        let server = ServerConfig {
            token: token.into(),
            ..ServerConfig::default()
        };
        let web = Arc::new(Web::new(
            db.clone(),
            &server,
            "127.0.0.1:8787".parse().unwrap(),
            Arc::new(Notify::new()),
            Arc::new(Notify::new()),
        ));
        Setup { _dir: dir, web, db }
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
