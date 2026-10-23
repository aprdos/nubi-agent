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

    /// Check if message has no_reply flag set
    pub fn no_reply(&self) -> bool {
        self.payload
            .metadata
            .as_ref()
            .and_then(|m| m.get("no_reply"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
