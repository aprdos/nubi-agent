use clap::Parser;
use nubi_agent::{Agent, AgentConfig, NubiClient};

#[derive(Parser)]
#[command(name = "nubi-agent")]
#[command(about = "Run Claude Code agents that respond to nubi room messages")]
#[command(version)]
struct Args {
    /// Nubi server URL
    #[arg(long, env = "NUBI_URL", default_value = "http://localhost:9099")]
    url: String,

    /// Space ID
    #[arg(long, env = "NUBI_SPACE")]
    space: String,

    /// Agent ID (your identity in the room)
    #[arg(long, env = "NUBI_AGENT")]
    agent: String,

    /// API token
    #[arg(long, env = "NUBI_TOKEN")]
    token: String,

    /// Room(s) to listen to, or "*" for all
    #[arg(long, default_value = "*")]
    rooms: String,

    /// System prompt for Claude
    #[arg(long)]
    system_prompt: Option<String>,

    /// Skip Claude permission prompts
    #[arg(long)]
    skip_permissions: bool,

    /// Number of recent messages to include as context
    #[arg(long, default_value = "10")]
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

    /// LLM backend for the spawned claude. Defaults to the local gateway on
    /// :5000, which is less aggressive than the raw Anthropic API.
    #[arg(long, env = "NUBI_LLM_BASE_URL", default_value = "http://localhost:5000")]
    llm_base_url: String,

    /// API key for the LLM backend (the gateway's kg- prefixed key).
    #[arg(long, env = "NUBI_LLM_KEY")]
    llm_key: Option<String>,

    /// Model for the spawned claude. The gateway needs dotted IDs
    /// (e.g. claude-opus-4.5), not the dashed form.
    #[arg(long, env = "NUBI_LLM_MODEL", default_value = "claude-opus-4.5")]
    model: String,
}

/// Derive the standard nubi-mcp URL from the engine URL: same host, port 8089,
/// path /mcp. Falls back to a localhost default if the engine URL has no host.
fn default_mcp_url(engine_url: &str) -> String {
    let (scheme, rest) = engine_url
        .split_once("://")
        .unwrap_or(("http", engine_url));
    let authority = rest.split('/').next().unwrap_or(rest);
    let host = authority.split(':').next().unwrap_or(authority);
    let host = if host.is_empty() { "localhost" } else { host };
    format!("{}://{}:8089/mcp", scheme, host)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let nubi = NubiClient::new(&args.url, &args.token, &args.space);

    let mcp_url = Some(args.mcp_url.unwrap_or_else(|| default_mcp_url(&args.url)));

    let config = AgentConfig {
        agent_id: args.agent,
        rooms: args.rooms,
        system_prompt: args.system_prompt,
        skip_permissions: args.skip_permissions,
        context_size: args.context_size,
        rate_limit_ms: args.rate_limit_ms,
        mcp_url,
        heartbeat_interval_secs: args.heartbeat_secs,
        llm_base_url: Some(args.llm_base_url),
        llm_api_key: args.llm_key,
        llm_model: Some(args.model),
    };

    let mut agent = Agent::new(nubi, config);
    agent.run().await
}

#[cfg(test)]
mod tests {
    use super::default_mcp_url;

    #[test]
    fn derives_mcp_url_from_engine() {
        assert_eq!(
            default_mcp_url("http://52.205.133.174:9099"),
            "http://52.205.133.174:8089/mcp"
        );
        assert_eq!(
            default_mcp_url("http://localhost:9099"),
            "http://localhost:8089/mcp"
        );
        assert_eq!(
            default_mcp_url("https://nubi.example.com"),
            "https://nubi.example.com:8089/mcp"
        );
        assert_eq!(
            default_mcp_url("http://localhost:9099/"),
            "http://localhost:8089/mcp"
        );
    }
}
