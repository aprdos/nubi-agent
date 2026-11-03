use clap::Parser;
use nubi_agent::harness::HarnessKind;
use nubi_agent::{Agent, AgentConfig, NubiClient};

/// The hosted engine. A token from the nubi app belongs to it.
const HOSTED_ENGINE: &str = "https://api.nubi.chat";

/// Hosted engines keep the nubi MCP server on its own subdomain, not on a
/// port next to the engine.
const HOSTED_MCP: &[(&str, &str)] = &[
    ("api.nubi.chat", "mcp.nubi.chat"),
    ("chat.nubi.chat", "mcp.nubi.chat"),
    ("chat.poimen.io", "mcp.poimen.io"),
];

const AFTER_HELP: &str = "\
Quick start:
  export NUBI_TOKEN=nubi_...   # nubi app: Agents > New agent (shown once)
  nubi-agent                   # joins the token's space on https://api.nubi.chat
                               # and answers with Claude Code (`claude`, logged in)

Then mention the agent in a room, or DM it. Nothing else is required: the
agent's name and its space come from the token.

Docs: https://github.com/aprdos/nubi-agent";

#[derive(Parser)]
#[command(name = "nubi-agent", version)]
#[command(
    about = "Put a coding agent (Claude Code, Codex, Gemini, ...) in your nubi rooms. It answers when mentioned or DMed."
)]
#[command(override_usage = "nubi-agent --token <TOKEN> [OPTIONS]")]
#[command(after_help = AFTER_HELP, after_long_help = AFTER_HELP)]
struct Args {
    /// The agent's token (nubi_...). It names the agent and its space. Get
    /// one in the nubi app: Agents > New agent
    #[arg(
        long,
        env = "NUBI_TOKEN",
        hide_env_values = true,
        help_heading = "Connect"
    )]
    token: Option<String>,

    /// The nubi engine. The hosted one is https://api.nubi.chat; a
    /// self-hosted engine is usually http://host:9099
    #[arg(long, env = "NUBI_URL", default_value = HOSTED_ENGINE, help_heading = "Connect")]
    url: String,

    /// Room(s) to listen to, or "*" for every room the agent is in
    #[arg(long, default_value = "*", help_heading = "Connect")]
    rooms: String,

    /// The nubi MCP server that gives the CLI its nubi tools (send, search,
    /// threads, ...). Defaults to the hosted engine's own MCP server, else
    /// the engine host on port 8089 at /mcp
    #[arg(long, env = "NUBI_MCP_URL", help_heading = "Connect")]
    mcp_url: Option<String>,

    /// Which coding-agent CLI answers messages. It must be installed and
    /// logged in (or given --llm-key)
    #[arg(
        long,
        env = "NUBI_HARNESS",
        value_enum,
        default_value_t = HarnessKind::Claude,
        help_heading = "Agent"
    )]
    harness: HarnessKind,

    /// Persona or standing instructions, appended to the built-in prompt
    #[arg(long, help_heading = "Agent")]
    system_prompt: Option<String>,

    /// Auto-approve every tool call the CLI makes (files, shell, ...).
    /// Without it the agent can still talk and use nubi tools
    #[arg(long, help_heading = "Agent")]
    skip_permissions: bool,

    /// Model for the CLI. Unset means the CLI's own default
    #[arg(long, env = "NUBI_LLM_MODEL", help_heading = "Model")]
    model: Option<String>,

    /// Model the CLI falls back to when the primary one is overloaded
    /// (Claude Code only)
    #[arg(long, env = "NUBI_LLM_FALLBACK_MODEL", help_heading = "Model")]
    fallback_model: Option<String>,

    /// Reasoning effort: low, medium, high, xhigh or max. Defaults to high
    /// with Claude Code; other CLIs get none
    #[arg(long, env = "NUBI_LLM_EFFORT", help_heading = "Model")]
    effort: Option<String>,

    /// Base URL of the LLM API the CLI talks to (ANTHROPIC_BASE_URL for
    /// Claude Code). Unset means the CLI's own login or key; set it to go
    /// through a gateway or proxy
    #[arg(long, env = "NUBI_LLM_BASE_URL", help_heading = "Model")]
    llm_base_url: Option<String>,

    /// API key for that LLM API (ANTHROPIC_API_KEY for Claude Code). Unset
    /// means the CLI's own login
    #[arg(
        long,
        env = "NUBI_LLM_KEY",
        hide_env_values = true,
        help_heading = "Model"
    )]
    llm_key: Option<String>,

    /// Who serves the model (anthropic, bedrock, vertex, ...), recorded with
    /// each reply's token usage. Read off --llm-base-url when unset
    #[arg(long, env = "NUBI_LLM_PROVIDER", help_heading = "Model")]
    llm_provider: Option<String>,

    /// Most recent room messages to include as context (the newest also
    /// win a fixed character budget)
    #[arg(long, default_value = "30", help_heading = "Tuning")]
    context_size: usize,

    /// Cooldown between replies in one room, in milliseconds
    #[arg(long, default_value = "5000", help_heading = "Tuning")]
    rate_limit_ms: u64,

    /// How often the typing indicator is refreshed while a turn runs, in
    /// seconds (0 sets it once and leaves it)
    #[arg(long, default_value = "5", help_heading = "Tuning")]
    heartbeat_secs: u64,

    /// Compact the CLI's conversation once it fills this percentage of the
    /// context window (Claude Code only; 0 leaves the CLI's own default)
    #[arg(
        long,
        env = "NUBI_COMPACT_AT_PCT",
        default_value = "60",
        help_heading = "Tuning"
    )]
    compact_at_pct: u8,

    /// Kill a run that takes longer than this many seconds and tell the room
    #[arg(
        long,
        env = "NUBI_TURN_TIMEOUT_SECS",
        default_value = "900",
        help_heading = "Tuning"
    )]
    turn_timeout_secs: u64,

    /// Answer nothing until the agent's mandate (the skill sets bound to it
    /// in the engine) has been fetched, and refuse a turn while it cannot
    /// be. Off by default: an engine without mandates simply means none
    #[arg(long, env = "NUBI_MANDATE_STRICT", help_heading = "Skills")]
    mandate_strict: bool,

    /// Never load skills from ~/.nubi/skills or the agent's own skills
    /// directory; only room skills and the mandate
    #[arg(long, env = "NUBI_NO_LOCAL_SKILLS", help_heading = "Skills")]
    no_local_skills: bool,
}

/// The nubi MCP server that goes with an engine URL: the hosted engines'
/// own subdomain, else the same host on port 8089 at /mcp (the standard
/// self-hosted nubi-mcp deployment). Localhost when the URL has no host.
fn default_mcp_url(engine_url: &str) -> String {
    let (scheme, rest) = engine_url.split_once("://").unwrap_or(("http", engine_url));
    let authority = rest.split('/').next().unwrap_or(rest);
    let host = authority.split(':').next().unwrap_or(authority);
    let host = if host.is_empty() { "localhost" } else { host };

    if let Some((_, mcp)) = HOSTED_MCP.iter().find(|(engine, _)| *engine == host) {
        return format!("{scheme}://{mcp}/mcp");
    }
    format!("{scheme}://{host}:8089/mcp")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let Some(token) = args.token.filter(|t| !t.trim().is_empty()) else {
        eprintln!(
            "nubi-agent: no token.\n\n\
             Pass --token nubi_... or set NUBI_TOKEN. The token names the agent and its space;\n\
             get one in the nubi app: Agents > New agent (it is shown once).\n\n\
             Run 'nubi-agent --help' for everything else."
        );
        std::process::exit(2);
    };

    // Cheap, and before the engine: a missing CLI is the most common
    // first-run problem, and it should not hide behind a connection retry.
    let bin = args.harness.harness().binary();
    if let Err(e) = tokio::process::Command::new(bin)
        .arg("--version")
        .output()
        .await
    {
        // `output` fails only when the process cannot be started; a CLI
        // that runs and exits non-zero is present, and the first turn
        // reports what it prints.
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow::bail!(
                "'{}' not found on PATH. Install it first:\n  {}\nor pick another CLI with --harness.",
                bin,
                args.harness.harness().install_hint()
            );
        }
        anyhow::bail!("Cannot run '{}': {}", bin, e);
    }

    let nubi = NubiClient::connect(&args.url, &token).await?;

    let agent_id = nubi.agent_id().to_string();
    let mcp_url = Some(args.mcp_url.unwrap_or_else(|| default_mcp_url(&args.url)));

    let llm_provider = args
        .llm_provider
        .clone()
        .unwrap_or_else(|| infer_provider(args.llm_base_url.as_deref(), args.harness));
    // Claude Code gets an effort level by default; everything else about the
    // model is the CLI's own unless set, so a logged-in CLI just works.
    let effort = match args.harness {
        HarnessKind::Claude => Some(args.effort.unwrap_or_else(|| "high".to_string())),
        _ => args.effort,
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
        llm_base_url: args.llm_base_url,
        llm_provider,
        llm_api_key: args.llm_key,
        llm_model: args.model,
        llm_fallback_model: args.fallback_model,
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

/// The provider behind an LLM base URL. Without one, the harness's own
/// vendor (`anthropic` for Claude Code, `openai` for Codex, `google` for
/// Gemini, else the CLI's name); `bedrock`/`vertex` when Claude Code is told
/// to use them (only Claude Code reads those); `anthropic` for Anthropic's
/// API, else `custom:<host>`.
fn infer_provider(base_url: Option<&str>, harness: HarnessKind) -> String {
    if harness == HarnessKind::Claude {
        if std::env::var_os("CLAUDE_CODE_USE_BEDROCK").is_some() {
            return "bedrock".to_string();
        }
        if std::env::var_os("CLAUDE_CODE_USE_VERTEX").is_some() {
            return "vertex".to_string();
        }
    }
    let Some(url) = base_url else {
        return match harness {
            HarnessKind::Claude => "anthropic",
            HarnessKind::Codex => "openai",
            HarnessKind::Gemini => "google",
            HarnessKind::Cursor => "cursor",
            HarnessKind::Opencode => "opencode",
            HarnessKind::Amp => "amp",
            HarnessKind::Copilot => "copilot",
        }
        .to_string();
    };
    let host = url
        .split("://")
        .last()
        .unwrap_or(url)
        .split(['/', '?'])
        .next()
        .unwrap_or_default()
        .to_lowercase();
    if host == "api.anthropic.com" {
        "anthropic".to_string()
    } else {
        format!("custom:{host}")
    }
}

#[cfg(test)]
mod tests {
    use super::HarnessKind;
    use super::{default_mcp_url, infer_provider, Args, HOSTED_ENGINE};
    use clap::{CommandFactory, Parser};

    #[test]
    fn provider_is_read_off_the_base_url() {
        assert_eq!(infer_provider(None, HarnessKind::Claude), "anthropic");
        assert_eq!(infer_provider(None, HarnessKind::Codex), "openai");
        assert_eq!(infer_provider(None, HarnessKind::Gemini), "google");
        assert_eq!(
            infer_provider(Some("https://api.anthropic.com"), HarnessKind::Codex),
            "anthropic"
        );
        assert_eq!(
            infer_provider(Some("https://API.anthropic.com/v1"), HarnessKind::Codex),
            "anthropic"
        );
        assert_eq!(
            infer_provider(Some("http://localhost:5000"), HarnessKind::Codex),
            "custom:localhost:5000"
        );
        assert_eq!(
            infer_provider(Some("https://openrouter.ai/api?x=1"), HarnessKind::Codex),
            "custom:openrouter.ai"
        );
    }

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
        // Hosted engines keep nubi-mcp on its own subdomain.
        assert_eq!(
            default_mcp_url("https://api.nubi.chat"),
            "https://mcp.nubi.chat/mcp"
        );
        assert_eq!(
            default_mcp_url("https://chat.poimen.io/"),
            "https://mcp.poimen.io/mcp"
        );
        assert_eq!(default_mcp_url(HOSTED_ENGINE), "https://mcp.nubi.chat/mcp");
    }

    /// The minimal invocation: a token, and every other default is one a
    /// newcomer can live with (hosted engine, the CLI's own model and
    /// login, no gateway).
    #[test]
    fn a_token_is_enough() {
        let args = Args::try_parse_from(["nubi-agent", "--token", "nubi_x"]).unwrap();
        assert_eq!(args.token.as_deref(), Some("nubi_x"));
        assert_eq!(args.url, HOSTED_ENGINE);
        assert_eq!(args.harness, HarnessKind::Claude);
        assert!(args.llm_base_url.is_none());
        assert!(args.llm_key.is_none());
        assert!(args.model.is_none());
        assert!(args.fallback_model.is_none());
        assert_eq!(args.rooms, "*");
    }

    /// The help text tells a newcomer the two things they need and never
    /// mentions one deployment's internals.
    #[test]
    fn help_explains_itself() {
        let help = Args::command().render_long_help().to_string();
        assert!(help.contains("Quick start:"), "{help}");
        assert!(help.contains("Agents > New agent"), "{help}");
        assert!(help.contains("https://api.nubi.chat"), "{help}");
        for heading in ["Connect:", "Agent:", "Model:", "Tuning:", "Skills:"] {
            assert!(help.contains(heading), "missing {heading}:\n{help}");
        }
        for leak in ["gateway on :5000", "kg-", "kiro"] {
            assert!(!help.contains(leak), "help leaks {leak:?}:\n{help}");
        }
        Args::command().debug_assert();
    }
}
