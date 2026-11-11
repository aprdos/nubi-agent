use serde::{Deserialize, Serialize};
use serde_json::Value;

/// SSE event data from nubi stream
#[derive(Debug, Clone, Deserialize)]
pub struct SseData {
    pub room_id: String,
    pub entry: LogEntry,
}

/// SSE `mandate_update`: the agent's resolved mandate changed.
#[derive(Debug, Clone, Deserialize)]
pub struct MandateUpdate {
    pub agent_id: String,
    #[serde(default)]
    pub revision: Option<u64>,
    pub hash: String,
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
    /// Set on `kind: "file"` parts.
    #[serde(default)]
    pub file: Option<FileRef>,
}

/// How a prompt names a file of this type: "image", "PDF" or "file".
pub fn file_kind_word(mime_type: &str) -> &'static str {
    if mime_type.starts_with("image/") {
        "image"
    } else if mime_type == "application/pdf" {
        "PDF"
    } else {
        "file"
    }
}

/// An uploaded attachment a message carries. The server fills name, type
/// and size from the upload record.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct FileRef {
    pub upload_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub mime_type: String,
    #[serde(default)]
    pub size: u64,
}

/// Outbound message for POST
#[derive(Debug, Clone, Serialize)]
pub struct SendMessage {
    pub message_id: String,
    pub parts: Vec<SendPart>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mentions: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
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

    /// The files this entry carries, in order.
    pub fn files(&self) -> Vec<FileRef> {
        self.payload
            .parts
            .iter()
            .filter(|p| p.kind == "file")
            .filter_map(|p| p.file.clone())
            .collect()
    }

    /// Check if this entry mentions a specific agent (or @here for all)
    pub fn mentions(&self, agent_id: &str) -> bool {
        let none = Vec::new();
        let list = self.payload.mentions.as_ref().unwrap_or(&none);
        nubi_agent_rules::mentions(agent_id, list, &self.text())
    }

    /// Whether `agent_id` should answer this entry, posted in `room_id`: the
    /// rule nubi-managed also uses to decide whether to start a sandbox.
    pub fn wants_reply(&self, agent_id: &str, room_id: &str) -> bool {
        let none = Vec::new();
        let text = self.text();
        nubi_agent_rules::wants_reply(
            agent_id,
            &nubi_agent_rules::Message {
                room: room_id,
                sender: &self.sender,
                mentions: self.payload.mentions.as_ref().unwrap_or(&none),
                text: &text,
                metadata: self.payload.metadata.as_ref(),
            },
        )
    }

    /// Agents named in `mentions`, if any.
    pub fn mention_list(&self) -> Vec<String> {
        self.payload
            .mentions
            .as_ref()
            .map(|m| m.iter().map(|s| normalize_id(s).to_string()).collect())
            .unwrap_or_default()
    }

    /// `metadata.cc`: who was copied, to see the message but not act on it.
    pub fn cc_list(&self) -> Vec<String> {
        self.payload
            .metadata
            .as_ref()
            .and_then(|m| m.get("cc"))
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| normalize_id(s).to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Check if this is a regular message (not system event)
    pub fn is_message(&self) -> bool {
        self.payload.payload_type == "message"
    }
}

use nubi_agent_rules::normalize_id;

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
                    file: None,
                }],
                mentions: mentions.map(|m| m.into_iter().map(String::from).collect()),
                metadata: None,
            },
        }
    }

    #[test]
    fn file_parts_are_read_and_text_ignores_them() {
        let entry: LogEntry = serde_json::from_value(json!({
            "global_index": 3,
            "sender": "alice",
            "timestamp_ms": 1,
            "payload": {
                "type": "message",
                "parts": [
                    {"kind": "file", "file": {"upload_id": "up_1", "name": "cat.png", "mime_type": "image/png", "size": 3}},
                    {"kind": "text", "text": "look"},
                    {"kind": "file", "file": {"upload_id": "up_2"}}
                ]
            }
        }))
        .unwrap();
        assert_eq!(entry.text(), "look");
        let files = entry.files();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].name, "cat.png");
        assert_eq!(files[1].upload_id, "up_2");
        assert_eq!(files[1].mime_type, "");
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
