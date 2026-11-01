//! A fake YouTube Data API serving the responses recorded by
//! tests/fixtures/youtube/record.py, matched on endpoint and parameters
//! (never the key). Shared by the unit tests and tests/youtube_api.rs, which
//! each use only some of it.
#![allow(dead_code)]

use std::path::Path;

use serde_json::Value;
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

pub const PLAYLIST: &str = "PLfSl2rOZ6Zto";
pub const UPLOADS: &str = "UU5eeDHYjTWF9pPWYDKpVMYQ";
pub const CHANNEL_ID: &str = "UC5eeDHYjTWF9pPWYDKpVMYQ";
pub const HANDLE: &str = "@supercombooficial";

fn recorded(name: &str) -> Value {
    let file = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/youtube/recorded")
        .join(format!("{name}.json"));
    serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap()
}

/// Serve every recorded 200 response.
pub async fn start() -> MockServer {
    let server = MockServer::start().await;
    let index: Vec<Value> = serde_json::from_value(recorded("index")).unwrap();
    for entry in index {
        let name = entry["name"].as_str().unwrap();
        let saved = recorded(name);
        if saved["status"] != 200 {
            continue;
        }
        let mut mock = Mock::given(method("GET"))
            .and(path(format!("/{}", entry["endpoint"].as_str().unwrap())));
        let params = entry["params"].as_object().unwrap();
        for (key, value) in params {
            mock = mock.and(query_param(key.as_str(), value.as_str().unwrap()));
        }
        if !params.contains_key("pageToken") && entry["endpoint"] == "playlistItems" {
            mock = mock.and(query_param_is_missing("pageToken"));
        }
        mock.respond_with(ResponseTemplate::new(200).set_body_json(&saved["body"]))
            .mount(&server)
            .await;
    }
    server
}

/// An API error in YouTube's shape, e.g. `error(403, "quotaExceeded", …)`.
/// Quota and server errors can't be recorded on demand; these follow
/// Google's documented format, like the recorded key-invalid response.
pub fn error(status: u16, reason: &str, message: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(serde_json::json!({
        "error": {
            "code": status,
            "message": message,
            "errors": [{ "message": message, "domain": "youtube.quota", "reason": reason }],
        }
    }))
}

/// The recorded response for a rejected key.
pub fn key_invalid() -> ResponseTemplate {
    let saved = recorded("key-invalid");
    ResponseTemplate::new(400).set_body_json(&saved["body"])
}

/// YouTube's /shorts/<id> page for every short upload of the channel, as
/// recorded (`shorts.json`): 200 for a Short, 303 to /watch for the rest.
pub async fn shorts() -> MockServer {
    use wiremock::matchers::path_regex;
    let server = MockServer::start().await;
    let answers: std::collections::BTreeMap<String, String> =
        serde_json::from_value(recorded("shorts")).unwrap();
    for (id, kind) in answers {
        let response = if kind == "short" {
            ResponseTemplate::new(200)
        } else {
            ResponseTemplate::new(303)
                .insert_header("location", format!("https://www.youtube.com/watch?v={id}"))
        };
        Mock::given(method("HEAD"))
            .and(path(format!("/shorts/{id}")))
            .respond_with(response)
            .mount(&server)
            .await;
    }
    // Anything else (longer videos aren't asked about).
    Mock::given(method("HEAD"))
        .and(path_regex("^/shorts/"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    server
}
