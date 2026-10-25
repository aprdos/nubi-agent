use serde::{Deserialize, Serialize};
use serde_json::Value;

/// SSE event data from nubi stream
#[derive(Debug, Clone, Deserialize)]
pub struct SseData {
    pub room_id: String,
    pub entry: LogEntry,
}

/// A single log entry from the room
#[derive(Debug, Clone, Deserialize)]
pub struct LogEntry {
    pub global_index: u64,
    pub sender: String,
    pub timestamp_ms: u64,
    pub payload: Payload,
}

/// Message payload
#[derive(Debug, Clone, Deserialize)]
pub struct Payload {
    #[serde(rename = "type")]
    pub payload_type: String,
    #[serde(default)]
    pub parts: Vec<Part>,
    #[serde(default)]
    pub mentions: Option<Vec<String>>,
    #[serde(default)]
    pub metadata: Option<Value>,
}

/// Message part
#[derive(Debug, Clone, Deserialize)]
pub struct Part {
    pub kind: String,
    pub text: Option<String>,
}

/// Outbound message for POST
#[derive(Debug, Serialize)]
pub struct SendMessage {
    pub message_id: String,
    pub parts: Vec<SendPart>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mentions: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct SendPart {
    pub kind: &'static str,
    pub text: String,
}

impl LogEntry {
    /// Extract text content from all parts
    pub fn text(&self) -> String {
        self.payload
            .parts
            .iter()
            .filter_map(|p| p.text.as_deref())
            .collect::<Vec<_>>()
            .join("")
    }

    /// Check if this entry mentions a specific agent (or @here for all)
    pub fn mentions(&self, agent_id: &str) -> bool {
        if let Some(mentions) = &self.payload.mentions {
            if mentions.iter().any(|m| m == agent_id || m == "here") {
                return true;
            }
        }
        let text = self.text();
        if text.contains("@here") {
            return true;
        }
        // Check for exact @mention (not substring match)
        let pattern = format!("@{}", agent_id);
        if let Some(pos) = text.find(&pattern) {
            let after = pos + pattern.len();
            // Match only if followed by non-alphanumeric or end of string
            if after >= text.len() {
                return true;
            }
            let next_char = text[after..].chars().next().unwrap();
            if !next_char.is_alphanumeric() && next_char != '-' && next_char != '_' {
                return true;
            }
        }
        false
    }

    /// Check if this is a regular message (not system event)
    pub fn is_message(&self) -> bool {
        self.payload.payload_type == "message"
    }

    /// Whether this agent is expected to answer.
    ///
    /// Fails open: anything missing or malformed means "go ahead". A wrong
    /// `false` is a silent deadlock — the sender waits for an answer that
    /// never comes and nothing logs why — while a wrong `true` is just noise.
    /// Only a well-formed list that leaves us out keeps us quiet, plus the
    /// explicit empty list, which means nobody should reply.
    pub fn expects_reply(&self, agent_id: &str) -> bool {
        let Some(value) = self
            .payload
            .metadata
            .as_ref()
            .and_then(|m| m.get("expect_reply"))
        else {
            return true;
        };
        let Some(arr) = value.as_array() else {
            return true;
        };
        if arr.is_empty() {
            return false;
        }

        let ids: Vec<&str> = arr
            .iter()
            .filter_map(|v| v.as_str())
            .map(normalize_id)
            .filter(|s| !s.is_empty())
            .collect();
        if ids.is_empty() {
            return true;
        }

        let me = normalize_id(agent_id);
        ids.iter().any(|id| id.eq_ignore_ascii_case(me))
    }
}

fn normalize_id(raw: &str) -> &str {
    raw.trim().trim_start_matches('@').trim()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_entry(sender: &str, text: &str, mentions: Option<Vec<&str>>) -> LogEntry {
        LogEntry {
            global_index: 1,
            sender: sender.to_string(),
            timestamp_ms: 1000,
            payload: Payload {
                payload_type: "message".to_string(),
                parts: vec![Part {
                    kind: "text".to_string(),
                    text: Some(text.to_string()),
                }],
                mentions: mentions.map(|m| m.into_iter().map(String::from).collect()),
                metadata: None,
            },
        }
    }

    fn with_metadata(metadata: Value) -> LogEntry {
        let mut entry = make_entry("alice", "hey bot", Some(vec!["bot"]));
        entry.payload.metadata = Some(metadata);
        entry
    }

    #[test]
    fn test_expects_reply_absent_or_malformed() {
        // No metadata at all.
        assert!(make_entry("alice", "hey bot", Some(vec!["bot"])).expects_reply("bot"));
        // Metadata present but no expect_reply key.
        assert!(with_metadata(json!({"other": 1})).expects_reply("bot"));
        assert!(with_metadata(json!({"expect_reply": null})).expects_reply("bot"));
        // Wrong type, and a list with nothing usable in it: fail open.
        assert!(with_metadata(json!({"expect_reply": "yes"})).expects_reply("bot"));
        assert!(with_metadata(json!({"expect_reply": [""]})).expects_reply("bot"));
        assert!(with_metadata(json!({"expect_reply": [7]})).expects_reply("bot"));
    }

    #[test]
    fn test_expects_reply_empty_list_silences_everyone() {
        let entry = with_metadata(json!({"expect_reply": []}));
        assert!(!entry.expects_reply("bot"));
        assert!(!entry.expects_reply("other"));
    }

    #[test]
    fn test_expects_reply_targets_named_agent() {
        let entry = with_metadata(json!({"expect_reply": ["bot"]}));
        assert!(entry.expects_reply("bot"));
        assert!(!entry.expects_reply("other"));
    }

    #[test]
    fn test_expects_reply_normalizes_ids() {
        let entry = with_metadata(json!({"expect_reply": [" @Bot "]}));
        assert!(entry.expects_reply("bot"));
        assert!(entry.expects_reply("@bot"));
        assert!(!entry.expects_reply("other"));
    }

    #[test]
    fn test_extract_text() {
        let entry = make_entry("alice", "hello world", None);
        assert_eq!(entry.text(), "hello world");
    }

    #[test]
    fn test_mentions_via_field() {
        let entry = make_entry("alice", "hey bot", Some(vec!["bot"]));
        assert!(entry.mentions("bot"));
        assert!(!entry.mentions("other"));
    }

    #[test]
    fn test_mentions_via_text() {
        let entry = make_entry("alice", "hey @bot how are you", None);
        assert!(entry.mentions("bot"));
        assert!(!entry.mentions("other"));

        // Ensure no substring matching
        let entry2 = make_entry("alice", "@claude-ec2-2 hello", None);
        assert!(entry2.mentions("claude-ec2-2"));
        assert!(!entry2.mentions("claude-ec2")); // should NOT match substring
    }

    #[test]
    fn test_mentions_here() {
        let entry = make_entry("alice", "@here anyone around?", None);
        assert!(entry.mentions("bot"));
        assert!(entry.mentions("any-agent"));

        let entry2 = make_entry("alice", "hey", Some(vec!["here"]));
        assert!(entry2.mentions("bot"));
    }

    #[test]
    fn test_is_message() {
        let entry = make_entry("alice", "hello", None);
        assert!(entry.is_message());

        let system = LogEntry {
            global_index: 1,
            sender: "system".to_string(),
            timestamp_ms: 1000,
            payload: Payload {
                payload_type: "system".to_string(),
                parts: vec![],
                mentions: None,
                metadata: None,
            },
        };
        assert!(!system.is_message());
    }
}
