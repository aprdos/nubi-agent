//! The README quick start, end to end: `examples/quickstart.sh` starts the
//! real binary with nothing but a token and an engine URL, the agent connects,
//! a DM arrives on the stream, and a reply is posted.
//!
//! The engine is a few axum routes in this file, in the shapes the real one
//! uses; `claude` is a shell script on PATH that prints the stream-json a
//! Claude Code run prints. So this proves the wiring (flags, /v1/me, SSE,
//! spawn, parse, post), not the model.
#![cfg(unix)]

use std::convert::Infallible;
use std::os::unix::fs::PermissionsExt;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::stream::{self, Stream, StreamExt};
use serde_json::{json, Value};

const SPACE: &str = "demo";
const AGENT: &str = "helper";
const TOKEN: &str = "nubi_quickstart_token";
const REPLY: &str = "hello from the quick start";

#[derive(Default)]
struct Engine {
    /// Authorization header of every `GET /v1/me`.
    me_auth: Vec<Option<String>>,
    /// Every posted message: (room, body).
    posts: Vec<(String, Value)>,
}

type Shared = State<Arc<Mutex<Engine>>>;

async fn me(State(s): Shared, headers: HeaderMap) -> impl IntoResponse {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    s.lock().unwrap().me_auth.push(auth.clone());
    if auth.as_deref() != Some(&format!("Bearer {TOKEN}")) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": {"code": "invalid_credentials"}})),
        );
    }
    (
        StatusCode::OK,
        Json(json!({"space_id": SPACE, "agent_id": AGENT})),
    )
}

/// One DM to the agent, dated now so it is not read as replayed history,
/// then an open stream: the agent keeps listening, the test kills it.
async fn space_stream() -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let entry = json!({
        "room_id": format!("dm.alice.{AGENT}"),
        "entry": {
            "global_index": 1,
            "sender": "alice",
            "timestamp_ms": now_ms,
            "payload": {
                "type": "message",
                "parts": [{"kind": "text", "text": "hi, are you there?"}],
                "mentions": [AGENT]
            }
        }
    });
    let first =
        stream::once(async move { Ok(Event::default().event("entry").data(entry.to_string())) });
    Sse::new(first.chain(stream::pending())).keep_alive(KeepAlive::default())
}

async fn post_message(
    State(s): Shared,
    Path((_space, room)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    s.lock().unwrap().posts.push((room, body));
    (StatusCode::CREATED, Json(json!({"global_index": 2})))
}

async fn start_engine(state: Arc<Mutex<Engine>>) -> String {
    let app = Router::new()
        .route("/v1/me", get(me))
        .route("/v1/spaces/{space}/stream", get(space_stream))
        .route(
            "/v1/spaces/{space}/rooms",
            get(|| async { Json(json!([])) }),
        )
        .route(
            "/v1/spaces/{space}/agents/{agent}/mandate",
            get(|| async { StatusCode::NOT_FOUND }),
        )
        .route(
            "/v1/spaces/{space}/rooms/{room}/messages",
            post(post_message).get(|| async { Json(json!({"entries": []})) }),
        )
        .route(
            "/v1/spaces/{space}/rooms/{room}/typing",
            post(|| async { StatusCode::OK }),
        )
        .route(
            "/v1/spaces/{space}/rooms/{room}/offset",
            post(|| async { Json(json!({"committed": 0})) }),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// A `claude` that answers `--version`, and otherwise prints the stream a
/// `claude -p --output-format stream-json` run prints: init, one assistant
/// turn that calls StructuredOutput with the reply, its result, the final
/// result object.
fn fake_claude(dir: &std::path::Path) {
    let reply = json!({"text": REPLY, "mentions": ["alice"], "expect_reply": [], "post": true});
    let lines = [
        json!({"type": "system", "subtype": "init", "session_id": "s-1"}),
        json!({"type": "assistant", "message": {"id": "m-1", "model": "fake-model",
            "usage": {"input_tokens": 10, "output_tokens": 5},
            "content": [{"type": "tool_use", "id": "t-1", "name": "StructuredOutput", "input": reply}]}}),
        json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "t-1", "is_error": false}]}}),
        json!({"type": "result", "subtype": "success", "is_error": false, "result": REPLY,
            "session_id": "s-1", "num_turns": 1, "duration_ms": 1, "total_cost_usd": 0.0,
            "structured_output": reply}),
    ];
    let mut script = String::from(
        "#!/bin/sh\nfor a in \"$@\"; do [ \"$a\" = --version ] && { echo '0.0.0 (fake)'; exit 0; }; done\n",
    );
    for line in lines {
        // Single-quoted for the shell, so a quote inside the JSON must close,
        // escape and reopen the quoting.
        let escaped = line.to_string().replace('\'', "'\\''");
        script.push_str(&format!("echo '{}'\n", escaped));
    }
    let path = dir.join("claude");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[tokio::test]
async fn the_readme_quick_start_gets_a_reply() {
    let state = Arc::new(Mutex::new(Engine::default()));
    let engine_url = start_engine(Arc::clone(&state)).await;

    let home = tempfile::tempdir().unwrap();
    let bin_dir = home.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    fake_claude(&bin_dir);
    let real_bin = std::path::Path::new(env!("CARGO_BIN_EXE_nubi-agent"));
    let path = format!(
        "{}:{}:{}",
        bin_dir.display(),
        real_bin.parent().unwrap().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/quickstart.sh");
    let log = std::fs::File::create(home.path().join("agent.log")).unwrap();

    // Exactly the quick start: a token and (because this engine is not the
    // hosted one) its URL. Nothing about models, keys or gateways.
    let mut child = tokio::process::Command::new("sh")
        .arg(script)
        .env_clear()
        .env("PATH", path)
        .env("HOME", home.path())
        .env("TMPDIR", home.path())
        .env("NUBI_TOKEN", TOKEN)
        .env("NUBI_URL", &engine_url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(30);
    let posts = loop {
        let posts = state.lock().unwrap().posts.clone();
        if !posts.is_empty() {
            break posts;
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!(
                "nubi-agent exited early ({status}):\n{}",
                agent_log(home.path())
            );
        }
        assert!(
            Instant::now() < deadline,
            "no reply within 30s:\n{}",
            agent_log(home.path())
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let _ = child.kill().await;
    let _ = child.wait().await;

    let (room, body) = &posts[0];
    assert_eq!(room, &format!("dm.alice.{AGENT}"), "{body}");
    assert_eq!(body["parts"][0]["text"], REPLY, "{body}");
    assert_eq!(body["mentions"], json!(["alice"]), "{body}");
    // Who must act is `mentions` alone; replies no longer carry expect_reply.
    assert!(body["metadata"].get("expect_reply").is_none(), "{body}");

    let me_auth = state.lock().unwrap().me_auth.clone();
    assert_eq!(me_auth, vec![Some(format!("Bearer {TOKEN}"))]);

    let log = agent_log(home.path());
    assert!(
        log.contains(&format!("Connected as '{AGENT}' in space '{SPACE}'")),
        "{log}"
    );
    assert!(log.contains(&format!("Mention @{AGENT}")), "{log}");
    assert!(home
        .path()
        .join(".nubi-agents")
        .join(AGENT)
        .join("CLAUDE.md")
        .exists());
}

/// Without a token the script stops before starting anything, and says so.
#[test]
fn the_quick_start_asks_for_a_token() {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/quickstart.sh");
    let out = std::process::Command::new("sh")
        .arg(script)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("NUBI_TOKEN"), "{err}");
    assert!(err.contains("Agents > New agent"), "{err}");
}

/// The binary itself, with no token: a message that says what to set and
/// where it comes from, not clap's "required argument" line.
#[test]
fn the_binary_explains_a_missing_token() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_nubi-agent"))
        .env_clear()
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("--token"), "{err}");
    assert!(err.contains("NUBI_TOKEN"), "{err}");
    assert!(err.contains("Agents > New agent"), "{err}");
}

fn agent_log(home: &std::path::Path) -> String {
    std::fs::read_to_string(home.join("agent.log")).unwrap_or_default()
}
