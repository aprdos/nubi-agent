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
            .route("/v1/spaces/{space}/rooms/{room}/typing", post(ok))
            .route("/v1/spaces/{space}/rooms/{room}/offset", post(offset))
            .with_state(Arc::clone(&state));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
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
) -> impl IntoResponse {
    s.lock().unwrap().posts.push((room, body));
    (StatusCode::CREATED, Json(json!({"global_index": 1})))
}

async fn history() -> Json<Value> {
    Json(json!({"entries": []}))
}

async fn offset() -> Json<Value> {
    Json(json!({"committed": 0}))
}

async fn ok() -> StatusCode {
    StatusCode::OK
}
