//! `nubi-agent hook pre-tool-use`, as Claude Code runs it: the call on
//! stdin, the verdict in the exit code, the reason on stderr.

use std::io::Write;
use std::process::{Command, Stdio};

fn run(scope: &std::path::Path, input: &str) -> (i32, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_nubi-agent"))
        .args(["hook", "pre-tool-use", "--scope"])
        .arg(scope)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn send(room: &str) -> String {
    serde_json::json!({
        "session_id": "s1",
        "hook_event_name": "PreToolUse",
        "tool_name": "mcp__nubi__nubi_send",
        "tool_input": { "room_id": room, "text": "hi" }
    })
    .to_string()
}

#[test]
fn the_hook_denies_a_room_outside_the_scope_and_allows_the_rest() {
    let dir = std::env::temp_dir().join(format!("nubi-hook-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let scope = dir.join("general.json");
    std::fs::write(
        &scope,
        r#"{"allow":["thread.general.4","general","thread.general.*"],"room":"thread.general.4","family":"general"}"#,
    )
    .unwrap();

    let (code, err) = run(&scope, &send("pr-review"));
    assert_eq!(code, 2, "{err}");
    assert!(
        err.contains(
            r#"room_id "pr-review" is outside this conversation; send to "thread.general.4""#
        ),
        "{err}"
    );

    assert_eq!(run(&scope, &send("thread.general.9")).0, 0);
    assert_eq!(run(&scope, &send("general")).0, 0);
    // Another tool, or no scope file: allowed.
    assert_eq!(run(&scope, r#"{"tool_name":"Bash","tool_input":{}}"#).0, 0);
    let (code, err) = run(&dir.join("missing.json"), &send("pr-review"));
    assert_eq!(code, 0);
    assert!(err.contains("no scope"), "{err}");
}
