use eventsource_client::Client;
use futures::StreamExt;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::buffer::RoomBuffer;
use crate::claude::Claude;
use crate::nubi::NubiClient;
use crate::types::SseData;

/// Configuration for the agent
#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub agent_id: String,
    pub rooms: String,
    pub system_prompt: Option<String>,
    pub skip_permissions: bool,
    pub context_size: usize,
    pub rate_limit_ms: u64,
    pub mcp_url: Option<String>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            agent_id: String::new(),
            rooms: "*".to_string(),
            system_prompt: None,
            skip_permissions: false,
            context_size: 10,
            rate_limit_ms: 5000,
            mcp_url: None,
        }
    }
}

/// The main agent that listens to nubi and responds via Claude
pub struct Agent {
    config: AgentConfig,
    nubi: NubiClient,
    claude: Claude,
    buffer: RoomBuffer,
    start_time_ms: u64,
    mcp_config_path: Option<PathBuf>,
}

impl Agent {
    pub fn new(nubi: NubiClient, config: AgentConfig) -> Self {
        // Generate MCP config if mcp_url is provided
        let mcp_config_path = config.mcp_url.as_ref().map(|mcp_url| {
            Self::generate_mcp_config(&config.agent_id, nubi.token(), mcp_url)
        });

        let claude = Claude::new(&config.agent_id)
            .with_skip_permissions(config.skip_permissions)
            .with_system_prompt(config.system_prompt.clone())
            .with_mcp_config(mcp_config_path.as_deref());

        let buffer = RoomBuffer::new()
            .with_context_size(config.context_size)
            .with_rate_limit_ms(config.rate_limit_ms);

        let start_time_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        Self {
            config,
            nubi,
            claude,
            buffer,
            start_time_ms,
            mcp_config_path,
        }
    }

    /// Generate MCP config file for this agent
    fn generate_mcp_config(agent_id: &str, token: &str, mcp_url: &str) -> PathBuf {
        let config = serde_json::json!({
            "mcpServers": {
                "nubi": {
                    "type": "http",
                    "url": mcp_url,
                    "headers": {
                        "Authorization": format!("Bearer {}", token)
                    }
                }
            }
        });

        let path = PathBuf::from(format!("/tmp/mcp-{}.json", agent_id));
        fs::write(&path, serde_json::to_string_pretty(&config).unwrap())
            .expect("Failed to write MCP config");

        eprintln!("[nubi-agent] Generated MCP config at {:?}", path);
        path
    }

    /// Run the agent (blocking)
    pub async fn run(&mut self) -> anyhow::Result<()> {
        // Verify setup
        Claude::check_available().await?;
        self.nubi.verify().await?;

        eprintln!(
            "[nubi-agent] Connected as '{}', listening for mentions...",
            self.config.agent_id
        );

        let sse_url = self.nubi.stream_url(&self.config.rooms);
        let es = eventsource_client::ClientBuilder::for_url(&sse_url)?
            .header("Authorization", &format!("Bearer {}", self.nubi.token()))?
            .build();

        let mut stream = es.stream();

        while let Some(event) = stream.next().await {
            match event {
                Ok(eventsource_client::SSE::Event(ev)) if ev.event_type == "entry" => {
                    if let Err(e) = self.handle_event(&ev.data).await {
                        eprintln!("[nubi-agent] Error: {}", e);
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("[nubi-agent] SSE error: {}, reconnecting...", e);
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        }

        Ok(())
    }

    async fn handle_event(&mut self, data: &str) -> anyhow::Result<()> {
        let sse_data: SseData = serde_json::from_str(data)?;
        let room_id = &sse_data.room_id;
        let entry = &sse_data.entry;

        // Always buffer messages for context
        if entry.is_message() {
            self.buffer.push(room_id, entry);
        }

        // Skip historical messages
        if entry.timestamp_ms < self.start_time_ms {
            return Ok(());
        }

        // Skip own messages
        if entry.sender == self.config.agent_id {
            return Ok(());
        }

        // Skip non-message events
        if !entry.is_message() {
            return Ok(());
        }

        // Check if mentioned (skip check for DMs - always respond)
        let is_dm = room_id.starts_with("dm.");
        if !is_dm && !entry.mentions(&self.config.agent_id) {
            return Ok(());
        }

        // Skip messages with no_reply flag (prevents agent loops)
        if entry.no_reply() {
            return Ok(());
        }

        // Check rate limit
        if self.buffer.is_rate_limited(room_id) {
            eprintln!("[nubi-agent] Rate limited in {}, skipping", room_id);
            return Ok(());
        }

        let text = entry.text();
        eprintln!(
            "[nubi-agent] @{} in {}: {}",
            self.config.agent_id,
            room_id,
            text.chars().take(50).collect::<String>()
        );

        // Build prompt with context
        let context = self.buffer.context(room_id);
        let prompt = self.claude.build_prompt(
            &self.config.agent_id,
            room_id,
            &entry.sender,
            &text,
            &context,
        );

        // Run Claude
        eprintln!("[nubi-agent] Running claude...");
        let response = self.claude.run(&prompt).await?;

        if response.is_empty() {
            eprintln!("[nubi-agent] Empty response, skipping");
            return Ok(());
        }

        // Skip posting short confirmations (likely after tool use)
        let trimmed = response.trim();
        let trimmed_lower = trimmed.to_lowercase();
        let skip_words = ["ok", "done", "sent", "yes", "no", "(empty)", "empty", "."];
        if trimmed_lower.len() <= 10 && skip_words.iter().any(|w| trimmed_lower == *w) {
            eprintln!("[nubi-agent] Short confirmation '{}', skipping post", trimmed);
            return Ok(());
        }

        // Skip responses with prompt format leaking (e.g., "Human: ...")
        if trimmed.starts_with("Human:") || trimmed.starts_with("Assistant:") {
            eprintln!("[nubi-agent] Prompt format leaked '{}', skipping post", &trimmed[..trimmed.len().min(50)]);
            return Ok(());
        }

        // Post response
        eprintln!("[nubi-agent] Responding ({} chars)", response.len());
        self.nubi.post(room_id, &response).await?;
        self.buffer.mark_responded(room_id);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_config_default() {
        let config = AgentConfig::default();
        assert_eq!(config.rooms, "*");
        assert_eq!(config.context_size, 10);
        assert_eq!(config.rate_limit_ms, 5000);
        assert!(!config.skip_permissions);
    }
}
