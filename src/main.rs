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

    /// Most recent room messages to include as context (the newest win a
    /// fixed character budget as well)
    #[arg(long, default_value = "30")]
    context_size: usize,

    /// Rate limit between responses per room (ms)
    #[arg(long, default_value = "5000")]
    rate_limit_ms: u64,

    /// MCP server URL for nubi tools. Defaults to the engine host on port 8089
    /// at /mcp (the standard nubi-mcp deployment).
    #[arg(long, env = "NUBI_MCP_URL")]
    mcp_url: Option<String>,

    /// How often the typing indicator is refreshed while a turn runs, in
    /// seconds (0 sets it once and leaves it)
    #[arg(long, default_value = "5")]
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
    /// claude-opus-4-5, the newest Opus the local gateway's upstream serves.
    /// Use the dashed form: Claude Code hangs on a dotted id such as
    /// claude-opus-4.5. Other harnesses use their own default unless set.
    #[arg(long, env = "NUBI_LLM_MODEL")]
    model: Option<String>,

    /// Model the CLI falls back to when the primary one is overloaded
    /// (Claude Code only). Defaults to claude-sonnet-4-6 with --harness claude.
    #[arg(long, env = "NUBI_LLM_FALLBACK_MODEL")]
    fallback_model: Option<String>,

    /// Reasoning effort for the spawned CLI: low, medium, high, xhigh or max.
    /// Defaults to high with --harness claude; other CLIs get none.
    #[arg(long, env = "NUBI_LLM_EFFORT")]
    effort: Option<String>,

    /// Compact the CLI's conversation once it fills this percentage of the
    /// context window (Claude Code only). 0 leaves the CLI's own default.
    #[arg(long, env = "NUBI_COMPACT_AT_PCT", default_value = "60")]
    compact_at_pct: u8,

    /// Kill a harness run that takes longer than this many seconds and tell
    /// the room.
    #[arg(long, env = "NUBI_TURN_TIMEOUT_SECS", default_value = "900")]
    turn_timeout_secs: u64,

    /// Do not answer anything until the agent's mandate (the skill sets
    /// bound to it) has been fetched from the engine, and refuse a turn
    /// while it cannot be. Off by default: an engine without mandates
    /// simply means no mandate.
    #[arg(long, env = "NUBI_MANDATE_STRICT")]
    mandate_strict: bool,

    /// Never load skills from ~/.nubi/skills or the agent's own skills
    /// directory; only room skills and the mandate.
    #[arg(long, env = "NUBI_NO_LOCAL_SKILLS")]
    no_local_skills: bool,
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

    let (llm_base_url, model, fallback_model, effort) = match args.harness {
        HarnessKind::Claude => (
            Some(
                args.llm_base_url
                    .unwrap_or_else(|| "http://localhost:5000".to_string()),
            ),
            Some(args.model.unwrap_or_else(|| "claude-opus-4-5".to_string())),
            Some(
                args.fallback_model
                    .unwrap_or_else(|| "claude-sonnet-4-6".to_string()),
            ),
            Some(args.effort.unwrap_or_else(|| "high".to_string())),
        ),
        _ => (
            args.llm_base_url,
            args.model,
            args.fallback_model,
            args.effort,
        ),
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
        llm_fallback_model: fallback_model,
        llm_effort: effort,
        compact_at_pct: (args.compact_at_pct > 0).then_some(args.compact_at_pct.min(100)),
        turn_timeout_secs: args.turn_timeout_secs,
        harness: args.harness,
        mandate_strict: args.mandate_strict,
        no_local_skills: args.no_local_skills,
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
