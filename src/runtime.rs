use std::collections::BTreeMap;

use async_trait::async_trait;

use crate::buffer::BufferedMessage;
use crate::mandate::Mandate;
use crate::skills::Skill;

/// A message addressed to the agent that it has not answered yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trigger {
    pub sender: String,
    pub text: String,
    /// The message's global index in its room, so the model can thread on
    /// it or scroll back from it.
    pub index: u64,
    /// `metadata.cc`: copied, not asked to act.
    pub cc: Vec<String>,
    pub timestamp_ms: u64,
    /// Files the message carries, downloaded where possible.
    pub attachments: Vec<Attachment>,
}

/// A file on a message the agent is answering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub name: String,
    pub mime_type: String,
    /// Where it was saved in the workspace, or None if it could not be fetched.
    pub path: Option<std::path::PathBuf>,
}

/// The message a thread room was opened on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadAnchor {
    pub parent_room: String,
    pub index: u64,
    pub sender: String,
    pub text: String,
}

/// Everything one turn is built from. The runtime renders it into a prompt.
#[derive(Debug, Clone, Default)]
pub struct TurnInput {
    pub room_id: String,
    /// All pending messages for the room, oldest first. Never empty.
    pub triggers: Vec<Trigger>,
    /// Room history before the triggers, oldest first. When a session is
    /// resumed this holds only what arrived since the agent's last turn.
    pub context: Vec<BufferedMessage>,
    pub thread: Option<ThreadAnchor>,
    /// For a thread: what was said in the room it was opened from since the
    /// thread's last turn (or since it opened), oldest first.
    pub parent_context: Vec<BufferedMessage>,
    /// The agent's own recent turns in this room, as one line each.
    pub journal: Vec<String>,
    /// The agent's other conversations, one line each: turns running right
    /// now in other rooms, then recent turns there, related rooms first.
    pub elsewhere: Vec<String>,
    /// The harness session for this room is being continued, so the model
    /// already saw everything up to the agent's last turn.
    pub resumed: bool,
}

/// The harness conversation to run a turn in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    /// Continue an existing conversation rather than start one with this id.
    pub resume: bool,
}

/// A reply the model shaped itself instead of leaving routing to the harness.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StructuredReply {
    pub text: String,
    /// Who is asked to act on it (the To). Empty wakes nobody.
    pub mentions: Vec<String>,
    /// Who should see it without acting (the Cc).
    pub cc: Vec<String>,
    /// False when the model already delivered its answer via tools and
    /// nothing should be posted.
    pub post: bool,
}

/// What one run of the LLM produced.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RunOutput {
    /// The model's answer as plain text (empty when it only used tools).
    pub text: String,
    /// The answer in structured form when the harness supports it.
    pub reply: Option<StructuredReply>,
    /// Hash of the mandate that was in the system prompt for this run (see
    /// [`AgentRuntime::mandate_hash`]), read together with the prompt so the
    /// two cannot drift apart. None when no mandate was injected.
    pub mandate_hash: Option<String>,
    /// The CLI finished but reported a failure (API error, max turns, ...).
    pub is_error: bool,
    pub error: Option<String>,
    /// Model turns the CLI took, when it says.
    pub num_turns: Option<u64>,
    /// The CLI's conversation id, for resuming.
    pub session_id: Option<String>,
    pub cost_usd: Option<f64>,
    pub duration_ms: Option<u64>,
    /// The run's total, for the turn log.
    pub usage: TokenUsage,
    /// The same tokens split by the model that used them, as the CLI names
    /// it. A run can use several (a small model for side tasks, a fallback).
    pub model_usage: BTreeMap<String, TokenUsage>,
}

/// Tokens one run used, as far as the CLI reports them. Fields the CLI
/// leaves out stay None.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TokenUsage {
    pub input: Option<u64>,
    pub output: Option<u64>,
    /// Prompt tokens served from the cache.
    pub cache_read: Option<u64>,
    /// Prompt tokens written to the cache.
    pub cache_write: Option<u64>,
}

impl RunOutput {
    /// The run ended without the model producing any reply at all, as
    /// opposed to answering through tools over several turns.
    pub fn is_silent(&self) -> bool {
        !self.is_error
            && self.text.trim().is_empty()
            && self
                .reply
                .as_ref()
                .is_none_or(|r| r.post && r.text.trim().is_empty())
            && self.num_turns.unwrap_or(1) <= 1
    }
}

/// A runtime that can execute LLM invocations.
///
/// Different providers (Claude Code, OpenAI, Ollama, etc.) implement this trait
/// to provide a consistent interface for the agent to spawn LLM processes.
#[async_trait]
pub trait AgentRuntime: Send + Sync {
    /// Check if the runtime is available (CLI installed, API reachable, etc.)
    async fn check_available(&self) -> anyhow::Result<()>;

    /// Render a turn into the prompt the LLM sees.
    fn build_prompt(&self, turn: &TurnInput) -> String;

    /// Run the LLM with the given prompt, in `session` when given, and return
    /// the response together with the hash of the mandate that was in the
    /// system prompt it used.
    async fn run(&self, prompt: &str, session: Option<&Session>) -> anyhow::Result<RunOutput>;

    /// Whether `run` honours [`Session`]s.
    fn supports_sessions(&self) -> bool {
        false
    }

    /// How to start a session that stays up for a room (see [`crate::live`]),
    /// when this runtime runs them. None: every turn is a [`Self::run`].
    fn live_command(&self, _session: Option<&Session>) -> Option<crate::live::LiveCommand> {
        None
    }

    /// What a live turn's `result` event says about the run (errors, the
    /// session id, usage). Its text is never posted: a live session answers
    /// with nubi_send.
    fn live_output(&self, _result: &serde_json::Value) -> RunOutput {
        RunOutput::default()
    }

    /// Whether this runtime supports MCP tools.
    fn supports_mcp(&self) -> bool;

    /// Replace the agent's mandate and its ad-hoc skills.
    ///
    /// The two are set together because ad-hoc skills are deduplicated
    /// against the mandate. Takes `&self` so a running agent can swap them
    /// between turns; a run already in flight keeps the prompt it started
    /// with. Runtimes that write the system prompt to disk rewrite it here,
    /// and on an error keep the previous mandate injected, so what
    /// [`Self::mandate_hash`] reports never runs ahead of what is on disk.
    fn inject_mandate(
        &self,
        mandate: Option<&Mandate>,
        adhoc_skills: &[Skill],
    ) -> anyhow::Result<()>;

    /// Hash of the mandate as currently injected, recomputed from the parsed
    /// skills the prompt section was rendered from (see
    /// [`crate::mandate::mandate_hash`]). None when no mandate is injected.
    fn mandate_hash(&self) -> Option<String>;
}
