//! The send guard: scope and dedup checks for nubi_send.
//!
//! A live session's MCP server is configured once per agent, so it cannot
//! tell one turn from another. Before each turn nubi-agent writes the turn's
//! scope to a file next to the session; a Claude Code `PreToolUse` hook
//! (`nubi-agent hook pre-tool-use --scope <file>`) reads it on every
//! nubi_send and refuses a room outside it, with a reason the model sees and
//! can correct in the same turn.
//!
//! **Scope check:** a turn people started may only send to its own family
//! (the room a thread hangs off, and that room's threads); a turn only
//! `system` started (GitHub events, workflow notices) may send anywhere.
//!
//! **Dedup check:** when a shared dedup file is configured, the hook also
//! refuses a send to a room that another turn already sent to within a short
//! window, *and* with the same content. This prevents the cross-room
//! double-post bug where two parallel turns independently post the same
//! message to the same destination.
//!
//! The check only applies to rooms outside the turn's own family: a reply
//! to the triggering room (or its parent/siblings) is never deduped, since
//! consecutive turns answering the same room is normal behavior.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::live::SEND_TOOL;

const DEDUP_TTL_MS: u64 = 30_000;

/// A turn's scope, as written to the scope file.
///
/// `allow` is `"*"` or a list of room ids, where an entry ending in `.*`
/// stands for the rooms one segment below it (`thread.general.*` is every
/// thread of `general`). `room` and `family` only word the refusal. `turn_id`
/// identifies this turn for dedup: sends from the same turn bypass the dedup
/// check.
pub fn scope(room_id: &str, family: &str, expects_reply: bool, turn_id: &str) -> Value {
    if !expects_reply {
        return json!({ "allow": "*", "room": room_id, "family": family, "turn_id": turn_id });
    }
    let mut allow = vec![room_id.to_string()];
    if family != room_id {
        allow.push(family.to_string());
    }
    allow.push(format!("thread.{family}.*"));
    json!({ "allow": allow, "room": room_id, "family": family, "turn_id": turn_id })
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

// ── Dedup state ──────────────────────────────────────────────────────────

/// One entry in the dedup file: who sent here, when, and what (hashed).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct DedupEntry {
    turn_id: String,
    ts_ms: u64,
    #[serde(default)]
    content_hash: u64,
}

/// Read the dedup file, pruning expired entries.
fn read_dedup(path: &Path) -> HashMap<String, DedupEntry> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return HashMap::new(),
    };
    let map: HashMap<String, DedupEntry> = match serde_json::from_str(&raw) {
        Ok(m) => m,
        Err(_) => return HashMap::new(),
    };
    let now = now_ms();
    map.into_iter()
        .filter(|(_, e)| now.saturating_sub(e.ts_ms) < DEDUP_TTL_MS)
        .collect()
}

/// Write the dedup file atomically.
fn write_dedup(path: &Path, map: &HashMap<String, DedupEntry>) -> std::io::Result<()> {
    let json = serde_json::to_string(map).expect("dedup map serializes");
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".send-dedup-{}.tmp", std::process::id()));
    std::fs::write(&tmp, json.as_bytes())?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Check and record a send in the dedup file. Returns `None` if allowed, or
/// a denial reason if another turn already sent the same content to this room
/// recently.
///
/// Sends to the turn's own family (`scope_family`) are never deduped — a turn
/// replying to its triggering room is normal, even if another turn just did
/// too.
///
/// Uses an exclusive file lock so two parallel hooks never race.
fn dedup_check(
    dedup_path: &Path,
    target_room: &str,
    turn_id: &str,
    content_hash: u64,
    scope_family: &str,
) -> Option<String> {
    if is_in_family(target_room, scope_family) {
        return None;
    }

    let lock_path = dedup_path.with_extension("lock");
    let dir = lock_path.parent().unwrap_or(Path::new("."));
    let _ = std::fs::create_dir_all(dir);

    let lock_file = match std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
    {
        Ok(f) => f,
        Err(_) => return None, // fail open
    };

    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        if unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return None; // fail open
        }
    }

    let mut map = read_dedup(dedup_path);
    if let Some(entry) = map.get(target_room) {
        if entry.turn_id != turn_id && entry.content_hash == content_hash {
            let reason = format!(
                "another turn already sent the same message to \"{target_room}\" in the last {} s; \
                 skip this send to avoid a double reply",
                DEDUP_TTL_MS / 1000
            );
            return Some(reason);
        }
    }
    map.insert(
        target_room.to_string(),
        DedupEntry {
            turn_id: turn_id.to_string(),
            ts_ms: now_ms(),
            content_hash,
        },
    );
    let _ = write_dedup(dedup_path, &map);
    None
}

/// Whether `room` belongs to the family rooted at `family`: the room itself,
/// the parent, or any thread of the parent.
fn is_in_family(room: &str, family: &str) -> bool {
    if room == family {
        return true;
    }
    let prefix = format!("thread.{family}.");
    if let Some(rest) = room.strip_prefix(&prefix) {
        return !rest.is_empty() && !rest.contains('.');
    }
    false
}

fn hash_text(text: &str) -> u64 {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut h = DefaultHasher::new();
    normalized.hash(&mut h);
    h.finish()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
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
/// `scope_path`. When `dedup_path` is set, also check whether another turn
/// already sent to this room recently. Returns the exit code (0 allow,
/// 2 deny) and what to print on stderr. Its own I/O failing (no scope file
/// yet, unreadable input) allows the call and says why.
pub fn pre_tool_use(
    scope_path: &Path,
    dedup_path: Option<&Path>,
    mut stdin: impl Read,
) -> (i32, Option<String>) {
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

    // Scope check (existing).
    match decide(&input, &scope) {
        Decision::Allow => {}
        Decision::Deny(reason) => return (2, Some(reason)),
    }

    // Dedup check: refuse if another turn just sent the same content here.
    // Sends to the turn's own family are skipped (normal reply behavior).
    if let (Some(dp), Some(target)) = (dedup_path, input["tool_input"]["room_id"].as_str()) {
        let turn_id = scope["turn_id"].as_str().unwrap_or("");
        let family = scope["family"].as_str().unwrap_or("");
        if !turn_id.is_empty() {
            let text = input["tool_input"]["text"].as_str().unwrap_or("");
            let ch = hash_text(text);
            if let Some(reason) = dedup_check(dp, target, turn_id, ch, family) {
                return (2, Some(reason));
            }
        }
    }

    (0, None)
}

/// Claude Code settings (for `--settings`) that run the hook before every
/// nubi_send. `exe` is this nubi-agent binary. When `dedup_path` is set, the
/// hook also runs the destination dedup check.
pub fn hook_settings(exe: &Path, scope_path: &Path, dedup_path: Option<&Path>) -> String {
    let mut command = format!(
        "{} hook pre-tool-use --scope {}",
        shell_quote(&exe.to_string_lossy()),
        shell_quote(&scope_path.to_string_lossy())
    );
    if let Some(dp) = dedup_path {
        command.push_str(&format!(" --dedup {}", shell_quote(&dp.to_string_lossy())));
    }
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
        let s = scope("general", "general", true, "t1");
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
        let s = scope("thread.general.7", "general", true, "t1");
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
        let s = scope("dm.a.b", "dm.a.b", true, "t1");
        assert_eq!(decide(&call("thread.dm.a.b.3"), &s), Decision::Allow);
        assert!(matches!(decide(&call("general"), &s), Decision::Deny(_)));
    }

    #[test]
    fn a_system_turn_may_send_anywhere() {
        let s = scope("github", "github", false, "t1");
        assert_eq!(s["allow"], "*");
        assert_eq!(decide(&call("pr-review"), &s), Decision::Allow);
    }

    #[test]
    fn other_tools_and_odd_calls_pass() {
        let s = scope("general", "general", true, "t1");
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
        let path = scope_file(&scope("general", "general", true, "t1"));
        let input = call("pr-review").to_string();
        let (code, err) = pre_tool_use(&path, None, input.as_bytes());
        assert_eq!(code, 2);
        assert!(err.unwrap().contains(r#"room_id "pr-review" is outside"#));
        let input = call("thread.general.3").to_string();
        assert_eq!(pre_tool_use(&path, None, input.as_bytes()), (0, None));
    }

    #[test]
    fn the_hook_allows_when_its_own_io_fails() {
        let missing = std::env::temp_dir().join(format!("nubi-none-{}", uuid::Uuid::new_v4()));
        let input = call("pr-review").to_string();
        let (code, err) = pre_tool_use(&missing, None, input.as_bytes());
        assert_eq!(code, 0);
        assert!(err.unwrap().contains("no scope"));
        let path = scope_file(&scope("general", "general", true, "t1"));
        assert_eq!(pre_tool_use(&path, None, &b"not json"[..]).0, 0);
    }

    #[test]
    fn the_settings_register_the_hook_on_nubi_send() {
        let s: Value = serde_json::from_str(&hook_settings(
            Path::new("/opt/nubi agent/nubi-agent"),
            Path::new("/tmp/x/it's.json"),
            None,
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

    #[test]
    fn the_settings_include_dedup_flag_when_path_given() {
        let s: Value = serde_json::from_str(&hook_settings(
            Path::new("/opt/nubi-agent"),
            Path::new("/tmp/scope.json"),
            Some(Path::new("/tmp/dedup.json")),
        ))
        .unwrap();
        let cmd = s["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(
            cmd.contains("--dedup"),
            "command should include --dedup: {cmd}"
        );
        assert!(
            cmd.contains("dedup.json"),
            "command should include dedup path: {cmd}"
        );
    }

    fn dedup_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("nubi-dedup-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn dedup_allows_first_send_and_blocks_same_content_from_different_turn() {
        let dir = dedup_dir();
        let dedup = dir.join("dedup.json");
        let h = hash_text("hi");
        // First turn sends to room-k: allowed.
        assert!(dedup_check(&dedup, "room-k", "turn-a", h, "other").is_none());
        // Same turn sends again: allowed (same turn_id).
        assert!(dedup_check(&dedup, "room-k", "turn-a", h, "other").is_none());
        // Different turn, same content to room-k: denied.
        let reason = dedup_check(&dedup, "room-k", "turn-b", h, "other");
        assert!(
            reason.is_some(),
            "same content from second turn should be denied"
        );
        assert!(reason.unwrap().contains("another turn already sent"));
        // Different room from second turn: allowed.
        assert!(dedup_check(&dedup, "room-m", "turn-b", h, "other").is_none());
    }

    #[test]
    fn dedup_allows_different_content_to_same_room() {
        let dir = dedup_dir();
        let dedup = dir.join("dedup.json");
        let h1 = hash_text("review PR #1");
        let h2 = hash_text("review PR #2");
        // First turn posts review for PR #1.
        assert!(dedup_check(&dedup, "pr-review", "turn-a", h1, "other").is_none());
        // Second turn posts different review for PR #2: allowed.
        assert!(dedup_check(&dedup, "pr-review", "turn-b", h2, "other").is_none());
    }

    #[test]
    fn dedup_skips_own_family() {
        let dir = dedup_dir();
        let dedup = dir.join("dedup.json");
        let h = hash_text("hi");
        // Turn-a sends to general (its own family room): recorded.
        assert!(dedup_check(&dedup, "general", "turn-a", h, "general").is_none());
        // Turn-b also sends to general with same content, its family too: allowed.
        assert!(dedup_check(&dedup, "general", "turn-b", h, "general").is_none());
        // Thread of the family: also skipped.
        assert!(dedup_check(&dedup, "thread.general.7", "turn-a", h, "general").is_none());
        assert!(dedup_check(&dedup, "thread.general.7", "turn-b", h, "general").is_none());
    }

    #[test]
    fn dedup_integrates_with_pre_tool_use() {
        let dir = dedup_dir();
        let dedup = dir.join("dedup.json");
        let scope_path = scope_file(&scope("github", "github", false, "turn-a"));
        // First send to room-k (system turn, scope allows anywhere): pass.
        let input = call("room-k").to_string();
        let (code, _) = pre_tool_use(&scope_path, Some(&dedup), input.as_bytes());
        assert_eq!(code, 0);
        // Second send from different turn, same content to same room: denied.
        let scope_b = scope_file(&scope("github", "github", false, "turn-b"));
        let (code, msg) = pre_tool_use(&scope_b, Some(&dedup), input.as_bytes());
        assert_eq!(code, 2);
        assert!(msg.unwrap().contains("another turn already sent"));
    }

    #[test]
    fn dedup_integrates_own_family_bypass() {
        let dir = dedup_dir();
        let dedup = dir.join("dedup.json");
        // Turn-a is in room general, sends to general.
        let scope_a = scope_file(&scope("general", "general", true, "turn-a"));
        let input = call("general").to_string();
        let (code, _) = pre_tool_use(&scope_a, Some(&dedup), input.as_bytes());
        assert_eq!(code, 0);
        // Turn-b is also in general, sends same content to general: allowed (own family).
        let scope_b = scope_file(&scope("general", "general", true, "turn-b"));
        let (code, _) = pre_tool_use(&scope_b, Some(&dedup), input.as_bytes());
        assert_eq!(code, 0);
    }

    #[test]
    fn dedup_entries_expire_after_ttl() {
        let dir = dedup_dir();
        let dedup = dir.join("dedup.json");
        let mut map = HashMap::new();
        map.insert(
            "room-old".to_string(),
            DedupEntry {
                turn_id: "turn-old".to_string(),
                ts_ms: now_ms().saturating_sub(DEDUP_TTL_MS + 1000),
                content_hash: hash_text("old"),
            },
        );
        write_dedup(&dedup, &map).unwrap();
        assert!(dedup_check(&dedup, "room-old", "turn-new", hash_text("old"), "other").is_none());
    }

    #[test]
    fn dedup_without_file_fails_open() {
        let dir = dedup_dir();
        let dedup = dir.join("nonexistent-subdir").join("dedup.json");
        assert!(dedup_check(&dedup, "room-k", "turn-a", hash_text("hi"), "other").is_none());
    }

    #[test]
    fn is_in_family_checks() {
        assert!(is_in_family("general", "general"));
        assert!(is_in_family("thread.general.7", "general"));
        assert!(!is_in_family("thread.general.7.sub", "general"));
        assert!(!is_in_family("pr-review", "general"));
        assert!(!is_in_family("thread.other.7", "general"));
        assert!(is_in_family("dm.a.b", "dm.a.b"));
        assert!(is_in_family("thread.dm.a.b.3", "dm.a.b"));
    }
}
