//! The send lock: which rooms a live turn may post to with nubi_send.
//!
//! A live session's MCP server is configured once per agent, so it cannot
//! tell one turn from another. Before each turn nubi-agent writes the turn's
//! scope to a file next to the session; a Claude Code `PreToolUse` hook
//! (`nubi-agent hook pre-tool-use --scope <file>`) reads it on every
//! nubi_send and refuses a room outside it, with a reason the model sees and
//! can correct in the same turn.
//!
//! A turn people started may only send to its own family (the room a thread
//! hangs off, and that room's threads); a turn only `system` started (GitHub
//! events, workflow notices) may send anywhere.

use std::io::Read;
use std::path::Path;

use serde_json::{json, Value};

use crate::live::SEND_TOOL;

/// A turn's scope, as written to the scope file.
///
/// `allow` is `"*"` or a list of room ids, where an entry ending in `.*`
/// stands for the rooms one segment below it (`thread.general.*` is every
/// thread of `general`). `room` and `family` only word the refusal.
pub fn scope(room_id: &str, family: &str, expects_reply: bool) -> Value {
    if !expects_reply {
        return json!({ "allow": "*", "room": room_id, "family": family });
    }
    let mut allow = vec![room_id.to_string()];
    if family != room_id {
        allow.push(family.to_string());
    }
    allow.push(format!("thread.{family}.*"));
    json!({ "allow": allow, "room": room_id, "family": family })
}

/// Write `scope` to `path` atomically (temp file, then rename), so the hook
/// never reads half a file.
pub fn write_scope(path: &Path, scope: &Value) -> anyhow::Result<()> {
    crate::harness::write_private(path, &scope.to_string())
}

fn allows(pattern: &str, room: &str) -> bool {
    if pattern == "*" || pattern == room {
        return true;
    }
    match pattern.strip_suffix('*') {
        Some(prefix) if prefix.ends_with('.') => room
            .strip_prefix(prefix)
            .is_some_and(|rest| !rest.is_empty() && !rest.contains('.')),
        _ => false,
    }
}

/// What the hook decides for one tool call.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// Refused, with the reason the model is told.
    Deny(String),
}

/// Decide a `PreToolUse` call from the hook input (`tool_name`,
/// `tool_input.room_id`) and the scope file's contents. Anything that is not
/// a nubi_send with a room, or a scope that is not readable, is allowed.
pub fn decide(input: &Value, scope: &Value) -> Decision {
    if input["tool_name"].as_str() != Some(SEND_TOOL) {
        return Decision::Allow;
    }
    let Some(target) = input["tool_input"]["room_id"].as_str() else {
        return Decision::Allow;
    };
    let ok = match &scope["allow"] {
        Value::String(p) => allows(p, target),
        Value::Array(list) => list
            .iter()
            .filter_map(Value::as_str)
            .any(|p| allows(p, target)),
        // Malformed: fail open, like a missing file.
        _ => true,
    };
    if ok {
        return Decision::Allow;
    }
    let room = scope["room"].as_str().unwrap_or("this room");
    let family = scope["family"].as_str().unwrap_or(room);
    let also = if family == room {
        "or a thread of it".to_string()
    } else {
        format!("or \"{family}\" and its threads")
    };
    Decision::Deny(format!(
        "room_id \"{target}\" is outside this conversation; send to \"{room}\" ({also})"
    ))
}

/// The hook itself: read the call from `stdin` and the scope from
/// `scope_path`. Returns the exit code (0 allow, 2 deny) and what to print
/// on stderr. Its own I/O failing (no scope file yet, unreadable input)
/// allows the call and says why.
pub fn pre_tool_use(scope_path: &Path, mut stdin: impl Read) -> (i32, Option<String>) {
    let mut raw = String::new();
    if let Err(e) = stdin.read_to_string(&mut raw) {
        return (0, Some(format!("nubi-agent hook: cannot read input: {e}")));
    }
    let Ok(input) = serde_json::from_str::<Value>(&raw) else {
        return (0, Some("nubi-agent hook: input is not JSON".into()));
    };
    if input["tool_name"].as_str() != Some(SEND_TOOL) {
        return (0, None);
    }
    let scope = match std::fs::read_to_string(scope_path) {
        Ok(s) => match serde_json::from_str::<Value>(&s) {
            Ok(v) => v,
            Err(e) => {
                return (
                    0,
                    Some(format!(
                        "nubi-agent hook: scope {} unreadable ({e}); allowing",
                        scope_path.display()
                    )),
                )
            }
        },
        Err(e) => {
            return (
                0,
                Some(format!(
                    "nubi-agent hook: no scope at {} ({e}); allowing",
                    scope_path.display()
                )),
            )
        }
    };
    match decide(&input, &scope) {
        Decision::Allow => (0, None),
        Decision::Deny(reason) => (2, Some(reason)),
    }
}

/// Claude Code settings (for `--settings`) that run the hook before every
/// nubi_send. `exe` is this nubi-agent binary.
pub fn hook_settings(exe: &Path, scope_path: &Path) -> String {
    let command = format!(
        "{} hook pre-tool-use --scope {}",
        shell_quote(&exe.to_string_lossy()),
        shell_quote(&scope_path.to_string_lossy())
    );
    json!({
        "hooks": {
            "PreToolUse": [{
                "matcher": SEND_TOOL,
                "hooks": [{ "type": "command", "command": command }]
            }]
        }
    })
    .to_string()
}

/// One shell word, single-quoted.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(room: &str) -> Value {
        json!({
            "hook_event_name": "PreToolUse",
            "tool_name": SEND_TOOL,
            "tool_input": { "room_id": room, "text": "hi" }
        })
    }

    #[test]
    fn a_person_s_turn_is_held_to_its_family() {
        let s = scope("general", "general", true);
        assert_eq!(
            s["allow"],
            json!(["general", "thread.general.*"]),
            "a room: itself and its threads"
        );
        assert_eq!(decide(&call("general"), &s), Decision::Allow);
        assert_eq!(decide(&call("thread.general.12"), &s), Decision::Allow);
        let Decision::Deny(why) = decide(&call("pr-review"), &s) else {
            panic!("allowed");
        };
        assert_eq!(
            why,
            r#"room_id "pr-review" is outside this conversation; send to "general" (or a thread of it)"#
        );
        // Not a thread of general, nor one of a room named like it.
        assert!(matches!(
            decide(&call("thread.general-2.4"), &s),
            Decision::Deny(_)
        ));
        assert!(matches!(
            decide(&call("thread.general.4.5"), &s),
            Decision::Deny(_)
        ));
    }

    #[test]
    fn a_thread_s_turn_may_send_to_its_parent_and_siblings() {
        let s = scope("thread.general.7", "general", true);
        assert_eq!(
            s["allow"],
            json!(["thread.general.7", "general", "thread.general.*"])
        );
        for ok in ["thread.general.7", "general", "thread.general.9"] {
            assert_eq!(decide(&call(ok), &s), Decision::Allow, "{ok}");
        }
        let Decision::Deny(why) = decide(&call("dm.a.b"), &s) else {
            panic!("allowed");
        };
        assert!(why.contains(r#"send to "thread.general.7" (or "general" and its threads)"#));
    }

    #[test]
    fn a_dm_family_is_its_own() {
        let s = scope("dm.a.b", "dm.a.b", true);
        assert_eq!(decide(&call("thread.dm.a.b.3"), &s), Decision::Allow);
        assert!(matches!(decide(&call("general"), &s), Decision::Deny(_)));
    }

    #[test]
    fn a_system_turn_may_send_anywhere() {
        let s = scope("github", "github", false);
        assert_eq!(s["allow"], "*");
        assert_eq!(decide(&call("pr-review"), &s), Decision::Allow);
    }

    #[test]
    fn other_tools_and_odd_calls_pass() {
        let s = scope("general", "general", true);
        assert_eq!(
            decide(&json!({"tool_name": "Bash", "tool_input": {}}), &s),
            Decision::Allow
        );
        assert_eq!(
            decide(&json!({"tool_name": SEND_TOOL, "tool_input": {}}), &s),
            Decision::Allow
        );
    }

    fn scope_file(v: &Value) -> std::path::PathBuf {
        let path = std::env::temp_dir()
            .join(format!("nubi-scope-{}", uuid::Uuid::new_v4()))
            .join("scope.json");
        write_scope(&path, v).unwrap();
        path
    }

    #[test]
    fn the_hook_exits_2_with_the_reason_on_a_room_outside() {
        let path = scope_file(&scope("general", "general", true));
        let input = call("pr-review").to_string();
        let (code, err) = pre_tool_use(&path, input.as_bytes());
        assert_eq!(code, 2);
        assert!(err.unwrap().contains(r#"room_id "pr-review" is outside"#));
        let input = call("thread.general.3").to_string();
        assert_eq!(pre_tool_use(&path, input.as_bytes()), (0, None));
    }

    #[test]
    fn the_hook_allows_when_its_own_io_fails() {
        let missing = std::env::temp_dir().join(format!("nubi-none-{}", uuid::Uuid::new_v4()));
        let input = call("pr-review").to_string();
        let (code, err) = pre_tool_use(&missing, input.as_bytes());
        assert_eq!(code, 0);
        assert!(err.unwrap().contains("no scope"));
        let path = scope_file(&scope("general", "general", true));
        assert_eq!(pre_tool_use(&path, &b"not json"[..]).0, 0);
    }

    #[test]
    fn the_settings_register_the_hook_on_nubi_send() {
        let s: Value = serde_json::from_str(&hook_settings(
            Path::new("/opt/nubi agent/nubi-agent"),
            Path::new("/tmp/x/it's.json"),
        ))
        .unwrap();
        let entry = &s["hooks"]["PreToolUse"][0];
        assert_eq!(entry["matcher"], SEND_TOOL);
        assert_eq!(entry["hooks"][0]["type"], "command");
        assert_eq!(
            entry["hooks"][0]["command"],
            r#"'/opt/nubi agent/nubi-agent' hook pre-tool-use --scope '/tmp/x/it'\''s.json'"#
        );
    }
}
