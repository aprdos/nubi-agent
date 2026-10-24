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

    /// MCP server URL for nubi tools (enables cross-room messaging)
    #[arg(long, env = "NUBI_MCP_URL")]
    mcp_url: Option<String>,

    /// Heartbeat interval in seconds (0 to disable)
    #[arg(long, default_value = "30")]
    heartbeat_secs: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let nubi = NubiClient::new(&args.url, &args.token, &args.space);

    let config = AgentConfig {
        agent_id: args.agent,
        rooms: args.rooms,
        system_prompt: args.system_prompt,
        skip_permissions: args.skip_permissions,
        context_size: args.context_size,
        rate_limit_ms: args.rate_limit_ms,
        mcp_url: args.mcp_url,
        heartbeat_interval_secs: args.heartbeat_secs,
    };

    let mut agent = Agent::new(nubi, config);
    agent.run().await
}
