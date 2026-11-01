use async_trait::async_trait;

use crate::buffer::BufferedMessage;
use crate::mandate::Mandate;

/// A runtime that can execute LLM invocations.
///
/// Different providers (Claude Code, OpenAI, Ollama, etc.) implement this trait
/// to provide a consistent interface for the agent to spawn LLM processes.
#[async_trait]
pub trait AgentRuntime: Send + Sync {
    /// Check if the runtime is available (CLI installed, API reachable, etc.)
    async fn check_available(&self) -> anyhow::Result<()>;

    /// Build the prompt with conversation context.
    ///
    /// The runtime decides how to format room context, sender info, and the
    /// new message into a prompt suitable for its LLM.
    fn build_prompt(
        &self,
        room_id: &str,
        sender: &str,
        message: &str,
        context: &[BufferedMessage],
    ) -> String;

    /// Run the LLM with the given prompt and return the response.
    async fn run(&self, prompt: &str) -> anyhow::Result<String>;

    /// Whether this runtime supports MCP tools.
    fn supports_mcp(&self) -> bool;

    /// Inject additional context into the system prompt.
    ///
    /// Called before run() to add skills, custom instructions, etc.
    fn inject_context(&mut self, context: &str);

    /// Replace the agent's mandate and its ad-hoc skills section.
    ///
    /// The two are set together because ad-hoc skills are deduplicated
    /// against the mandate. Takes `&self` so a running agent can swap them
    /// between turns; a run already in flight keeps the prompt it started
    /// with. Runtimes that write the system prompt to disk rewrite it here.
    fn inject_mandate(&self, mandate: Option<&Mandate>, adhoc_skills: &str) -> anyhow::Result<()>;

    /// Hash of the mandate as currently injected, recomputed from the skill
    /// text going into the prompt (see [`crate::mandate::mandate_hash`]).
    /// None when no mandate is injected.
    fn mandate_hash(&self) -> Option<String>;
}
