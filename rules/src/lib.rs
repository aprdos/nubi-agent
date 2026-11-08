//! When a nubi agent answers a message.
//!
//! nubi-agent decides with this, and so does nubi-managed when it chooses
//! whether to start a sandbox for a message. One copy of the rule keeps the
//! two from drifting apart (a managed agent that ignores `@here` while a
//! self-hosted one answers it). Keep it free of anything but `serde_json`, so
//! depending on it costs nothing.

use serde_json::Value;

/// One message, as much of it as the rule looks at.
#[derive(Debug, Clone, Copy)]
pub struct Message<'a> {
    /// The room it was posted in (`dm.<a>.<b>` for a direct message).
    pub room: &'a str,
    pub sender: &'a str,
    /// The payload's `mentions` list, as sent.
    pub mentions: &'a [String],
    /// The text of its parts, joined.
    pub text: &'a str,
    /// The payload's `metadata`, for `expect_reply`.
    pub metadata: Option<&'a Value>,
}

/// Whether `agent_id` should answer `msg`.
///
/// Never its own messages. In a DM, every message; in a room, one that
/// mentions it (`@here` mentions everyone). Either way, unless
/// `expect_reply` says someone else should answer.
pub fn wants_reply(agent_id: &str, msg: &Message) -> bool {
    if msg.sender == agent_id {
        return false;
    }
    if !is_dm(msg.room) && !mentions(agent_id, msg.mentions, msg.text) {
        return false;
    }
    expects_reply(agent_id, msg.metadata)
}

/// Direct-message rooms are `dm.<a>.<b>`.
pub fn is_dm(room: &str) -> bool {
    room.starts_with("dm.")
}

/// Whether a message names `agent_id`: in its `mentions` list or as an exact
/// `@id` in its text. `here` in the list or `@here` in the text names everyone.
pub fn mentions(agent_id: &str, mentions: &[String], text: &str) -> bool {
    if mentions.iter().any(|m| m == agent_id || m == "here") {
        return true;
    }
    if text.contains("@here") {
        return true;
    }
    // An exact @mention, not a prefix of a longer id. Every occurrence:
    // "@bot-x then @bot" names bot.
    let pattern = format!("@{agent_id}");
    text.match_indices(&pattern).any(
        |(pos, _)| match text[pos + pattern.len()..].chars().next() {
            None => true,
            Some(c) => !c.is_alphanumeric() && c != '-' && c != '_',
        },
    )
}

/// Whether `agent_id` is expected to answer, by `metadata.expect_reply`.
///
/// Fails open: anything missing or malformed means "go ahead". A wrong
/// `false` is a silent deadlock — the sender waits for an answer that never
/// comes and nothing logs why — while a wrong `true` is just noise. Only a
/// well-formed list that leaves us out keeps us quiet, plus the explicit
/// empty list, which means nobody should reply.
pub fn expects_reply(agent_id: &str, metadata: Option<&Value>) -> bool {
    let Some(value) = metadata.and_then(|m| m.get("expect_reply")) else {
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

/// An agent id as people write it: `@id`, with stray spaces.
pub fn normalize_id(raw: &str) -> &str {
    raw.trim().trim_start_matches('@').trim()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn msg<'a>(
        room: &'a str,
        sender: &'a str,
        mentions: &'a [String],
        text: &'a str,
    ) -> Message<'a> {
        Message {
            room,
            sender,
            mentions,
            text,
            metadata: None,
        }
    }

    fn list(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_room_needs_a_mention() {
        let none = list(&[]);
        assert!(!wants_reply(
            "bot",
            &msg("general", "alice", &none, "hello")
        ));
        let named = list(&["bot"]);
        assert!(wants_reply(
            "bot",
            &msg("general", "alice", &named, "hello")
        ));
        assert!(wants_reply(
            "bot",
            &msg("general", "alice", &none, "hey @bot, look")
        ));
        let other = list(&["other"]);
        assert!(!wants_reply(
            "bot",
            &msg("general", "alice", &other, "hey @other")
        ));
    }

    #[test]
    fn here_wakes_everyone() {
        let none = list(&[]);
        assert!(wants_reply(
            "bot",
            &msg("general", "alice", &none, "@here standup")
        ));
        let here = list(&["here"]);
        assert!(wants_reply(
            "bot",
            &msg("general", "alice", &here, "standup")
        ));
    }

    #[test]
    fn a_dm_needs_no_mention() {
        let none = list(&[]);
        assert!(wants_reply(
            "bot",
            &msg("dm.alice.bot", "alice", &none, "hi")
        ));
    }

    #[test]
    fn never_its_own_messages() {
        let here = list(&["here", "bot"]);
        assert!(!wants_reply(
            "bot",
            &msg("general", "bot", &here, "@here @bot")
        ));
        let none = list(&[]);
        assert!(!wants_reply(
            "bot",
            &msg("dm.alice.bot", "bot", &none, "hi")
        ));
    }

    #[test]
    fn expect_reply_can_hold_it_back() {
        let named = list(&["bot", "other"]);
        let meta = json!({"expect_reply": ["@other"]});
        let m = Message {
            metadata: Some(&meta),
            ..msg("general", "alice", &named, "")
        };
        assert!(!wants_reply("bot", &m));
        assert!(wants_reply("other", &m));

        let nobody = json!({"expect_reply": []});
        let dm = Message {
            metadata: Some(&nobody),
            ..msg("dm.alice.bot", "alice", &named, "fyi")
        };
        assert!(!wants_reply("bot", &dm));
    }

    #[test]
    fn exact_mentions_only() {
        assert!(!mentions("bot", &[], "@bot2 go"));
        assert!(!mentions("bot", &[], "@bot-x go"));
        assert!(!mentions("bot", &[], "@bot_x go"));
        assert!(mentions("bot", &[], "@bot, go"));
        assert!(mentions("bot", &[], "go @bot"));
        assert!(
            mentions("bot", &[], "@bot-x and @bot"),
            "a later exact one counts"
        );
        assert!(!mentions("bot", &[], "@bot-x and @botty"));
    }

    #[test]
    fn expect_reply_fails_open() {
        assert!(expects_reply("bot", None));
        assert!(expects_reply("bot", Some(&json!({}))));
        assert!(expects_reply("bot", Some(&json!({"expect_reply": "bot"}))));
        assert!(expects_reply(
            "bot",
            Some(&json!({"expect_reply": [1, ""]}))
        ));
        assert!(expects_reply(
            "Bot",
            Some(&json!({"expect_reply": [" @bot "]}))
        ));
    }
}
