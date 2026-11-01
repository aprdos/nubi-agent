use clap::Parser;
use nubi_agent::harness::HarnessKind;
use nubi_agent::{Agent, AgentConfig, NubiClient};

#[derive(Parser)]
#[command(name = "nubi-agent")]
#[command(
    about = "Run coding agents (Claude Code, Codex, Gemini, ...) that respond to nubi room messages"
)]
#[command(version)]
struct Args {
    /// Nubi server URL
    #[arg(long, env = "NUBI_URL", default_value = "https://chat.nubi.chat")]
    url: String,

    /// API token
    #[arg(long, env = "NUBI_TOKEN")]
    token: String,

    /// Room(s) to listen to, or "*" for all
    #[arg(long, default_value = "*")]
    rooms: String,

    /// Which coding-agent CLI answers messages
    #[arg(long, env = "NUBI_HARNESS", value_enum, default_value_t = HarnessKind::Claude)]
    harness: HarnessKind,

    /// Extra system prompt, appended to the built-in one
    #[arg(long)]
    system_prompt: Option<String>,

    /// Auto-approve every tool call the harness makes
    #[arg(long)]
    skip_permissions: bool,

    /// Number of recent messages to include as context
    #[arg(long, default_value = "30")]
    context_size: usize,

    /// Rate limit between responses per room (ms)
    #[arg(long, default_value = "5000")]
    rate_limit_ms: u64,

    /// MCP server URL for nubi tools. Defaults to the engine host on port 8089
    /// at /mcp (the standard nubi-mcp deployment).
    #[arg(long, env = "NUBI_MCP_URL")]
    mcp_url: Option<String>,

    /// Heartbeat interval in seconds (0 to disable)
    #[arg(long, default_value = "30")]
    heartbeat_secs: u64,

    /// LLM backend for the spawned CLI. With --harness claude this defaults
    /// to the local gateway on :5000, which is less aggressive than the raw
    /// Anthropic API. Other harnesses use their own default unless set.
    #[arg(long, env = "NUBI_LLM_BASE_URL")]
    llm_base_url: Option<String>,

    /// API key for the LLM backend (the gateway's kg- prefixed key).
    #[arg(long, env = "NUBI_LLM_KEY")]
    llm_key: Option<String>,

    /// Model for the spawned CLI. With --harness claude this defaults to
    /// claude-opus-4.5 (the gateway needs dotted IDs, not the dashed form).
    /// Other harnesses use their own default unless set.
    #[arg(long, env = "NUBI_LLM_MODEL")]
    model: Option<String>,
}

/// Derive the standard nubi-mcp URL from the engine URL: same host, port 8089,
/// path /mcp. Falls back to a localhost default if the engine URL has no host.
/// For the production domain (chat.nubi.chat), uses mcp.nubi.chat instead.
fn default_mcp_url(engine_url: &str) -> String {
    let (scheme, rest) = engine_url.split_once("://").unwrap_or(("http", engine_url));
    let authority = rest.split('/').next().unwrap_or(rest);
    let host = authority.split(':').next().unwrap_or(authority);
    let host = if host.is_empty() { "localhost" } else { host };

    // Production uses dedicated subdomains instead of port-based routing
    if host == "chat.nubi.chat" {
        return format!("{}://mcp.nubi.chat/mcp", scheme);
    }

    format!("{}://{}:8089/mcp", scheme, host)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let nubi = NubiClient::connect(&args.url, &args.token).await?;

    let agent_id = nubi.agent_id().to_string();
    let mcp_url = Some(args.mcp_url.unwrap_or_else(|| default_mcp_url(&args.url)));

    let (llm_base_url, model) = match args.harness {
        HarnessKind::Claude => (
            Some(
                args.llm_base_url
                    .unwrap_or_else(|| "http://localhost:5000".to_string()),
            ),
            Some(args.model.unwrap_or_else(|| "claude-opus-4.5".to_string())),
        ),
        _ => (args.llm_base_url, args.model),
    };

    let config = AgentConfig {
        agent_id,
        rooms: args.rooms,
        system_prompt: args.system_prompt,
        skip_permissions: args.skip_permissions,
        context_size: args.context_size,
        rate_limit_ms: args.rate_limit_ms,
        mcp_url,
        heartbeat_interval_secs: args.heartbeat_secs,
        llm_base_url,
        llm_api_key: args.llm_key,
        llm_model: model,
        harness: args.harness,
    };

    let mut agent = Agent::connect(nubi, config).await;
    agent.run().await
}

#[cfg(test)]
mod tests {
    use super::default_mcp_url;

    #[test]
    fn derives_mcp_url_from_engine() {
        assert_eq!(
            default_mcp_url("http://localhost:9099"),
            "http://localhost:8089/mcp"
        );
        assert_eq!(
            default_mcp_url("http://localhost:9099/"),
            "http://localhost:8089/mcp"
        );
        // Production domain uses dedicated subdomain
        assert_eq!(
            default_mcp_url("https://chat.nubi.chat"),
            "https://mcp.nubi.chat/mcp"
        );
    }
}
