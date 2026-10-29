use async_trait::async_trait;

use crate::buffer::BufferedMessage;

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
}
