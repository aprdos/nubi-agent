//! A stand-in nubi engine for tests, bound to `127.0.0.1:0`.
//!
//! Serves only the handful of routes nubi-agent calls, in the shapes the real
//! engine returns them, and records what it was sent so tests can assert on
//! request headers and posted bodies.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct MockState {
    /// Body of `GET /rooms?kind=skill`: the engine returns a bare array.
    pub skill_rooms: Vec<Value>,
    /// `GET /skills/{room}/prompt` per room: (content, version_index).
    pub prompts: HashMap<String, (String, u64)>,
    /// Resolve body for `GET /agents/{agent}/mandate`. None answers 404, the
    /// way an engine without skill sets does.
    pub mandate: Option<Value>,
    /// Answer every mandate request with this status instead (e.g. 500).
    pub mandate_fail: Option<u16>,
    /// Hold every mandate response this long, to let a test interleave
    /// something with an in-flight fetch.
    pub mandate_delay_ms: u64,
    /// The `If-None-Match` header of every mandate request, in order.
    pub mandate_requests: Vec<Option<String>>,
    /// Query strings of every room list request.
    pub room_queries: Vec<HashMap<String, String>>,
    /// Rooms whose prompt was fetched, in order.
    pub prompt_requests: Vec<String>,
    /// Every posted message: (room, body).
    pub posts: Vec<(String, Value)>,
    /// Every typing update: (room, typing), in order.
    pub typing: Vec<(String, bool)>,
    /// Statuses to answer the next posts with, in order, before accepting
    /// one. A 429 comes with `Retry-After: 0`.
    pub post_failures: Vec<u16>,
    /// Every post attempt: (room, message_id), refused ones included.
    pub post_attempts: Vec<(String, String)>,
    /// Statuses to answer the next `GET /v1/me` with, before answering it.
    pub me_failures: Vec<u16>,
    /// Committed offsets per room, kept monotonic as the engine does.
    pub offsets: HashMap<String, u64>,
    /// Attachment bytes by upload id. `/content` redirects to a storage
    /// route on a different host name, as the engine redirects to S3.
    pub uploads: HashMap<String, Vec<u8>>,
    /// The Authorization header each storage request arrived with.
    pub storage_auth: Vec<Option<String>>,
    /// Where `/content` redirects to; set by `start`.
    pub storage_base: String,
}

pub struct MockEngine {
    pub url: String,
    pub state: Arc<Mutex<MockState>>,
}

/// The ETag the engine sends for a resolve body: its hash, quoted.
pub fn etag_for(mandate: &Value) -> String {
    format!("\"{}\"", mandate["hash"].as_str().unwrap_or_default())
}

impl MockEngine {
    pub async fn start(state: MockState) -> Self {
        let state = Arc::new(Mutex::new(state));
        let app = Router::new()
            .route("/v1/spaces/{space}/rooms", get(list_rooms))
            .route("/v1/spaces/{space}/skills/{room}/prompt", get(skill_prompt))
            .route("/v1/spaces/{space}/agents/{agent}/mandate", get(mandate))
            .route(
                "/v1/spaces/{space}/rooms/{room}/messages",
                post(post_message).get(history),
            )
            .route("/v1/spaces/{space}/rooms/{room}/typing", post(typing))
            .route("/v1/spaces/{space}/rooms/{room}/offset", post(offset))
            .route("/v1/me", get(me))
            .route(
                "/v1/spaces/{space}/rooms/{room}/uploads/{upload}/content",
                get(upload_content),
            )
            .route("/storage/{upload}", get(storage))
            .with_state(Arc::clone(&state));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        state.lock().unwrap().storage_base = format!("http://localhost:{}", addr.port());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        Self {
            url: format!("http://{addr}"),
            state,
        }
    }

    pub fn state(&self) -> std::sync::MutexGuard<'_, MockState> {
        self.state.lock().unwrap()
    }
}

type Shared = State<Arc<Mutex<MockState>>>;

async fn list_rooms(State(s): Shared, Query(q): Query<HashMap<String, String>>) -> Json<Value> {
    let mut s = s.lock().unwrap();
    s.room_queries.push(q);
    Json(Value::Array(s.skill_rooms.clone()))
}

async fn skill_prompt(State(s): Shared, Path((_space, room)): Path<(String, String)>) -> Response {
    let mut s = s.lock().unwrap();
    s.prompt_requests.push(room.clone());
    match s.prompts.get(&room) {
        Some((content, idx)) => Json(json!({
            "content": content,
            "version_index": idx,
            "total_versions": 1,
            "is_pinned": false,
        }))
        .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no prompt versions found"})),
        )
            .into_response(),
    }
}

async fn mandate(State(s): Shared, headers: HeaderMap) -> Response {
    // Decide the answer under the lock, then release it before any delay so
    // the test can change the state while this response is in flight.
    let (response, delay_ms) = {
        let mut s = s.lock().unwrap();
        let inm = headers
            .get("if-none-match")
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        s.mandate_requests.push(inm.clone());
        (mandate_response(&s, inm.as_deref()), s.mandate_delay_ms)
    };
    if delay_ms > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
    }
    response
}

fn mandate_response(s: &MockState, inm: Option<&str>) -> Response {
    if let Some(code) = s.mandate_fail {
        return StatusCode::from_u16(code)
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
            .into_response();
    }

    let Some(body) = s.mandate.clone() else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response();
    };
    let etag = etag_for(&body);
    if inm == Some(etag.as_str()) {
        return (StatusCode::NOT_MODIFIED, [("etag", etag)]).into_response();
    }
    ([("etag", etag)], Json(body)).into_response()
}

async fn post_message(
    State(s): Shared,
    Path((_space, room)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Response {
    let mut s = s.lock().unwrap();
    let id = body["message_id"].as_str().unwrap_or_default().to_string();
    s.post_attempts.push((room.clone(), id));
    if !s.post_failures.is_empty() {
        let status = StatusCode::from_u16(s.post_failures.remove(0)).unwrap();
        return (status, [("retry-after", "0")], "nope").into_response();
    }
    s.posts.push((room, body));
    (StatusCode::CREATED, Json(json!({"global_index": 1}))).into_response()
}

async fn me(State(s): Shared) -> Response {
    let mut s = s.lock().unwrap();
    if !s.me_failures.is_empty() {
        let status = StatusCode::from_u16(s.me_failures.remove(0)).unwrap();
        return (status, [("retry-after", "0")], "busy").into_response();
    }
    Json(json!({"space_id": "s1", "agent_id": "support-bot"})).into_response()
}

async fn typing(
    State(s): Shared,
    Path((_space, room)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> StatusCode {
    let on = body["typing"].as_bool().unwrap_or(false);
    s.lock().unwrap().typing.push((room, on));
    StatusCode::OK
}

async fn upload_content(
    State(s): Shared,
    Path((_space, _room, upload)): Path<(String, String, String)>,
) -> Response {
    let s = s.lock().unwrap();
    if !s.uploads.contains_key(&upload) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "upload not found"})),
        )
            .into_response();
    }
    (
        StatusCode::TEMPORARY_REDIRECT,
        [("location", format!("{}/storage/{upload}", s.storage_base))],
    )
        .into_response()
}

async fn storage(State(s): Shared, Path(upload): Path<String>, headers: HeaderMap) -> Response {
    let mut s = s.lock().unwrap();
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    s.storage_auth.push(auth);
    match s.uploads.get(&upload) {
        Some(bytes) => ([("content-type", "image/png")], bytes.clone()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn history() -> Json<Value> {
    Json(json!({"entries": []}))
}

async fn offset(
    State(s): Shared,
    Path((_space, room)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let index = body["index"].as_u64().unwrap_or(0);
    let mut s = s.lock().unwrap();
    let committed = s.offsets.entry(room).or_insert(0);
    *committed = (*committed).max(index);
    Json(json!({"committed": *committed}))
}
