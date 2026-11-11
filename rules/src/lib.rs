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
    /// The payload's `metadata`, for `cc` and `chain`.
    pub metadata: Option<&'a Value>,
}

/// Agent messages in a row, with no person in between, after which a
/// mention from an agent wakes nobody. The engine counts them into
/// `metadata.chain` on every agent's message; a person speaking resets it.
pub const CHAIN_LIMIT: u64 = 100;

/// Whether `agent_id` should answer `msg`.
///
/// A mention is the signal to act, like an email's To: in a room, a message
/// that mentions the agent (`@here` mentions everyone); in a DM, every
/// message. `metadata.cc` is the Cc: names there see it but are not asked to
/// act, even when mentioned. Never its own messages, and never once agents
/// have answered each other `CHAIN_LIMIT` times without a person in between.
pub fn wants_reply(agent_id: &str, msg: &Message) -> bool {
    if msg.sender == agent_id {
        return false;
    }
    if chain_exhausted(msg.metadata) {
        return false;
    }
    if !is_dm(msg.room) && !mentions(agent_id, msg.mentions, msg.text) {
        return false;
    }
    !in_list(agent_id, msg.metadata, "cc")
}

/// The engine's count of agent messages in a row says this one comes after
/// too many: agents have been answering each other with no person around.
pub fn chain_exhausted(metadata: Option<&Value>) -> bool {
    metadata
        .and_then(|m| m.get("chain"))
        .and_then(Value::as_u64)
        .is_some_and(|n| n >= CHAIN_LIMIT)
}

/// Whether `metadata.<key>` lists `agent_id` (as an id or `@id`, any case).
fn in_list(agent_id: &str, metadata: Option<&Value>, key: &str) -> bool {
    let me = normalize_id(agent_id);
    metadata
        .and_then(|m| m.get(key))
        .and_then(Value::as_array)
        .is_some_and(|ids| {
            ids.iter()
                .filter_map(Value::as_str)
                .any(|id| normalize_id(id).eq_ignore_ascii_case(me))
        })
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
    at_name(text, "here") || at_name(text, agent_id)
}

/// Whether `text` has `@name` as a word of its own: not a prefix of a longer
/// name ("@bot-x then @bot" names bot, "@hereford" names nobody) and not
/// inside an address ("ops@here.com"). Every occurrence counts.
fn at_name(text: &str, name: &str) -> bool {
    let is_name_char = |c: char| c.is_alphanumeric() || c == '-' || c == '_';
    let pattern = format!("@{name}");
    text.match_indices(&pattern).any(|(pos, _)| {
        let before_ok = text[..pos]
            .chars()
            .next_back()
            .is_none_or(|c| !is_name_char(c) && c != '.');
        let after_ok = text[pos + pattern.len()..]
            .chars()
            .next()
            .is_none_or(|c| !is_name_char(c));
        before_ok && after_ok
    })
}

/// An agent id as people write it: `@id`, with stray spaces.
pub fn normalize_id(raw: &str) -> &str {
    raw.trim().trim_start_matches('@').trim()
}

#[cfg(test)]
mod to_and_cc_tests {
    use super::*;
    use serde_json::json;

    fn wants(agent: &str, room: &str, mentions: &[&str], text: &str, meta: Value) -> bool {
        let mentions: Vec<String> = mentions.iter().map(|s| s.to_string()).collect();
        let meta = (!meta.is_null()).then_some(meta);
        wants_reply(
            agent,
            &Message {
                room,
                sender: "alice",
                mentions: &mentions,
                text,
                metadata: meta.as_ref(),
            },
        )
    }

    #[test]
    fn a_mention_wakes_and_cc_only_informs() {
        assert!(wants("pine", "r", &["pine"], "take this", Value::Null));
        assert!(!wants("pine", "r", &[], "no one named", Value::Null));
        // Mentioned and copied: sees it, isn't asked.
        assert!(!wants(
            "pine",
            "r",
            &["pine", "apple"],
            "x",
            json!({"cc": ["pine"]})
        ));
        assert!(wants(
            "apple",
            "r",
            &["pine", "apple"],
            "x",
            json!({"cc": ["pine"]})
        ));
        assert!(!wants(
            "pine",
            "r",
            &["here"],
            "x",
            json!({"cc": ["@Pine"]})
        ));
        // In a DM everything is addressed to the agent, unless it's copied.
        assert!(wants("pine", "dm.alice.pine", &[], "hi", Value::Null));
        assert!(!wants(
            "pine",
            "dm.alice.pine",
            &[],
            "fyi",
            json!({"cc": ["pine"]})
        ));
    }

    #[test]
    fn long_agent_chains_stop_waking_anyone() {
        let at = |n: u64| wants("pine", "r", &["pine"], "x", json!({"chain": n}));
        assert!(at(1));
        assert!(at(CHAIN_LIMIT - 1));
        assert!(!at(CHAIN_LIMIT));
        assert!(!at(CHAIN_LIMIT + 5));
    }
}

#[cfg(test)]
mod at_name_tests {
    use super::*;

    #[test]
    fn here_and_ids_are_whole_words_in_text() {
        assert!(mentions("bot", &[], "@here standup"));
        assert!(mentions("bot", &[], "ping @here."));
        assert!(!mentions("bot", &[], "lunch at @hereford"));
        assert!(!mentions("bot", &[], "mail ops@here.com"));
        assert!(mentions("bot", &[], "@bot-x then @bot"));
        assert!(!mentions("bot", &[], "@bot-x only"));
        assert!(!mentions("bot", &[], "write to me@bot"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
