use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use crate::types::LogEntry;

const DEFAULT_BUFFER_SIZE: usize = 50;
const DEFAULT_CONTEXT_SIZE: usize = 10;
const DEFAULT_RATE_LIMIT_MS: u64 = 5000;

/// Buffered message for context
#[derive(Debug, Clone)]
pub struct BufferedMessage {
    pub sender: String,
    pub text: String,
    pub timestamp_ms: u64,
}

/// Per-room state
#[derive(Debug)]
struct RoomState {
    messages: VecDeque<BufferedMessage>,
    last_response: Option<Instant>,
}

impl RoomState {
    fn new() -> Self {
        Self {
            messages: VecDeque::new(),
            last_response: None,
        }
    }
}

/// Buffer for SSE messages per room with rate limiting
#[derive(Debug)]
pub struct RoomBuffer {
    rooms: HashMap<String, RoomState>,
    buffer_size: usize,
    context_size: usize,
    rate_limit_ms: u64,
}

impl Default for RoomBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl RoomBuffer {
    pub fn new() -> Self {
        Self {
            rooms: HashMap::new(),
            buffer_size: DEFAULT_BUFFER_SIZE,
            context_size: DEFAULT_CONTEXT_SIZE,
            rate_limit_ms: DEFAULT_RATE_LIMIT_MS,
        }
    }

    pub fn with_buffer_size(mut self, size: usize) -> Self {
        self.buffer_size = size;
        self
    }

    pub fn with_context_size(mut self, size: usize) -> Self {
        self.context_size = size;
        self
    }

    pub fn with_rate_limit_ms(mut self, ms: u64) -> Self {
        self.rate_limit_ms = ms;
        self
    }

    /// Add a message to the room buffer
    pub fn push(&mut self, room_id: &str, entry: &LogEntry) {
        let state = self
            .rooms
            .entry(room_id.to_string())
            .or_insert_with(RoomState::new);

        state.messages.push_back(BufferedMessage {
            sender: entry.sender.clone(),
            text: entry.text(),
            timestamp_ms: entry.timestamp_ms,
        });

        while state.messages.len() > self.buffer_size {
            state.messages.pop_front();
        }
    }

    /// Get recent messages for context (excludes the triggering message)
    pub fn context(&self, room_id: &str) -> Vec<BufferedMessage> {
        self.rooms
            .get(room_id)
            .map(|state| {
                let len = state.messages.len();
                let skip = len.saturating_sub(self.context_size + 1);
                let take = len.saturating_sub(1).saturating_sub(skip);
                state
                    .messages
                    .iter()
                    .skip(skip)
                    .take(take)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Check if we're rate limited for this room
    pub fn is_rate_limited(&self, room_id: &str) -> bool {
        self.rooms
            .get(room_id)
            .and_then(|state| state.last_response)
            .map(|last| last.elapsed().as_millis() < self.rate_limit_ms as u128)
            .unwrap_or(false)
    }

    /// Mark that we responded in this room
    pub fn mark_responded(&mut self, room_id: &str) {
        let state = self
            .rooms
            .entry(room_id.to_string())
            .or_insert_with(RoomState::new);
        state.last_response = Some(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Part, Payload};

    fn make_entry(sender: &str, text: &str, ts: u64) -> LogEntry {
        LogEntry {
            global_index: 1,
            sender: sender.to_string(),
            timestamp_ms: ts,
            payload: Payload {
                payload_type: "message".to_string(),
                parts: vec![Part {
                    kind: "text".to_string(),
                    text: Some(text.to_string()),
                }],
                mentions: None,
                metadata: None,
            },
        }
    }

    #[test]
    fn test_buffer_push_and_context() {
        let mut buffer = RoomBuffer::new().with_context_size(3);

        for i in 0..5 {
            buffer.push(
                "room1",
                &make_entry("alice", &format!("msg{}", i), i as u64),
            );
        }

        let ctx = buffer.context("room1");
        assert_eq!(ctx.len(), 3);
        assert_eq!(ctx[0].text, "msg1");
        assert_eq!(ctx[1].text, "msg2");
        assert_eq!(ctx[2].text, "msg3");
    }

    #[test]
    fn test_buffer_size_limit() {
        let mut buffer = RoomBuffer::new().with_buffer_size(3);

        for i in 0..10 {
            buffer.push(
                "room1",
                &make_entry("alice", &format!("msg{}", i), i as u64),
            );
        }

        let state = buffer.rooms.get("room1").unwrap();
        assert_eq!(state.messages.len(), 3);
        assert_eq!(state.messages[0].text, "msg7");
    }

    #[test]
    fn test_rate_limiting() {
        let mut buffer = RoomBuffer::new().with_rate_limit_ms(100);

        assert!(!buffer.is_rate_limited("room1"));

        buffer.mark_responded("room1");
        assert!(buffer.is_rate_limited("room1"));

        std::thread::sleep(std::time::Duration::from_millis(150));
        assert!(!buffer.is_rate_limited("room1"));
    }

    #[test]
    fn test_empty_room_context() {
        let buffer = RoomBuffer::new();
        let ctx = buffer.context("nonexistent");
        assert!(ctx.is_empty());
    }
}
