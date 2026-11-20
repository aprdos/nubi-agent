//! `nubi-agent hook pre-tool-use`, as Claude Code runs it: the call on
//! stdin, the verdict in the exit code, the reason on stderr.

use std::io::Write;
use std::process::{Command, Stdio};

fn run(scope: &std::path::Path, input: &str) -> (i32, String) {
    run_with_dedup(scope, None, input)
}

fn run_with_dedup(
    scope: &std::path::Path,
    dedup: Option<&std::path::Path>,
    input: &str,
) -> (i32, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nubi-agent"));
    cmd.args(["hook", "pre-tool-use", "--scope"]).arg(scope);
    if let Some(dp) = dedup {
        cmd.arg("--dedup").arg(dp);
    }
    let mut child = cmd
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

#[test]
fn dedup_blocks_same_content_from_different_turn_via_binary() {
    let dir = std::env::temp_dir().join(format!("nubi-dedup-hook-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let dedup = dir.join("dedup.json");

    // Scope: system turn (allow *), turn-a.
    let scope_a = dir.join("scope-a.json");
    std::fs::write(
        &scope_a,
        r#"{"allow":"*","room":"github","family":"github","turn_id":"turn-a"}"#,
    )
    .unwrap();

    // First send to room-k from turn-a: allowed.
    let (code, err) = run_with_dedup(&scope_a, Some(&dedup), &send("room-k"));
    assert_eq!(code, 0, "first send should pass: {err}");

    // Second send to room-k from turn-b, same content ("hi"): denied.
    let scope_b = dir.join("scope-b.json");
    std::fs::write(
        &scope_b,
        r#"{"allow":"*","room":"github","family":"github","turn_id":"turn-b"}"#,
    )
    .unwrap();
    let (code, err) = run_with_dedup(&scope_b, Some(&dedup), &send("room-k"));
    assert_eq!(
        code, 2,
        "same content from second turn should be denied: {err}"
    );
    assert!(
        err.contains("another turn already sent"),
        "reason should mention dedup: {err}"
    );

    // Same turn-a to room-k again: still allowed (same turn_id).
    let (code, err) = run_with_dedup(&scope_a, Some(&dedup), &send("room-k"));
    assert_eq!(code, 0, "same turn should still pass: {err}");

    // turn-b to a different room: allowed.
    let (code, err) = run_with_dedup(&scope_b, Some(&dedup), &send("room-m"));
    assert_eq!(code, 0, "different room should pass: {err}");

    // turn-b to room-k with different content: allowed.
    let different = serde_json::json!({
        "session_id": "s1",
        "hook_event_name": "PreToolUse",
        "tool_name": "mcp__nubi__nubi_send",
        "tool_input": { "room_id": "room-k", "text": "completely different message" }
    })
    .to_string();
    let (code, err) = run_with_dedup(&scope_b, Some(&dedup), &different);
    assert_eq!(code, 0, "different content should pass: {err}");

    let _ = std::fs::remove_dir_all(&dir);
}

fn send_text(room: &str, text: &str) -> String {
    serde_json::json!({
        "session_id": "s1",
        "hook_event_name": "PreToolUse",
        "tool_name": "mcp__nubi__nubi_send",
        "tool_input": { "room_id": room, "text": text }
    })
    .to_string()
}

#[test]
fn dedup_interleaving_via_binary() {
    let dir = std::env::temp_dir().join(format!("nubi-dedup-hook-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let dedup = dir.join("dedup.json");

    let scope_a = dir.join("scope-a.json");
    std::fs::write(
        &scope_a,
        r#"{"allow":"*","room":"github","family":"github","turn_id":"turn-a"}"#,
    )
    .unwrap();
    let scope_b = dir.join("scope-b.json");
    std::fs::write(
        &scope_b,
        r#"{"allow":"*","room":"github","family":"github","turn_id":"turn-b"}"#,
    )
    .unwrap();
    let scope_c = dir.join("scope-c.json");
    std::fs::write(
        &scope_c,
        r#"{"allow":"*","room":"github","family":"github","turn_id":"turn-c"}"#,
    )
    .unwrap();

    // A posts X to pr-review.
    let (code, err) = run_with_dedup(&scope_a, Some(&dedup), &send_text("pr-review", "review X"));
    assert_eq!(code, 0, "turn-a first send should pass: {err}");

    // C posts Y to pr-review (different content, allowed).
    let (code, err) = run_with_dedup(&scope_c, Some(&dedup), &send_text("pr-review", "review Y"));
    assert_eq!(code, 0, "turn-c different content should pass: {err}");

    // B posts X to pr-review — same content as A, must be denied.
    let (code, err) = run_with_dedup(&scope_b, Some(&dedup), &send_text("pr-review", "review X"));
    assert_eq!(
        code, 2,
        "turn-b same content as A after interleave should be denied: {err}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn dedup_allows_own_family_via_binary() {
    let dir = std::env::temp_dir().join(format!("nubi-dedup-hook-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let dedup = dir.join("dedup.json");

    // Turn-a in room general, sends to general.
    let scope_a = dir.join("scope-a.json");
    std::fs::write(
        &scope_a,
        r#"{"allow":["general","thread.general.*"],"room":"general","family":"general","turn_id":"turn-a"}"#,
    )
    .unwrap();
    let (code, err) = run_with_dedup(&scope_a, Some(&dedup), &send("general"));
    assert_eq!(code, 0, "own family should pass: {err}");

    // Turn-b also in general, sends same content to general: allowed (own family bypass).
    let scope_b = dir.join("scope-b.json");
    std::fs::write(
        &scope_b,
        r#"{"allow":["general","thread.general.*"],"room":"general","family":"general","turn_id":"turn-b"}"#,
    )
    .unwrap();
    let (code, err) = run_with_dedup(&scope_b, Some(&dedup), &send("general"));
    assert_eq!(code, 0, "own family from second turn should pass: {err}");

    let _ = std::fs::remove_dir_all(&dir);
}
