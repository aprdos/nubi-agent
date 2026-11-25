use clap::Parser;
use nubi_agent::harness::HarnessKind;
use nubi_agent::{Agent, AgentConfig, LiveScope, NubiClient};

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

    /// Env vars (comma-separated names) the CLI may see on top of the basics
    /// and its own credentials, e.g. SSH_AUTH_SOCK,AWS_PROFILE. Nothing else
    /// from this environment reaches it
    #[arg(
        long,
        env = "NUBI_PASS_ENV",
        value_delimiter = ',',
        help_heading = "Agent"
    )]
    pass_env: Vec<String>,

    /// In a room the agent has never read, answer mentions from this time
    /// (Unix ms) on instead of only those after it started. For launchers
    /// that start the agent because of a mention it must not skip.
    #[arg(long, env = "NUBI_ANSWER_SINCE_MS", help_heading = "Agent")]
    answer_since_ms: Option<u64>,

    /// Skip mentions older than the start (or --answer-since-ms) in every
    /// room, not only rooms never read. For launchers that answer the older
    /// ones themselves
    #[arg(long, env = "NUBI_ANSWER_SINCE_ALL_ROOMS", help_heading = "Agent")]
    answer_since_all_rooms: bool,

    /// Answer people only: leave mentions from bots (other agents) alone
    #[arg(long, env = "NUBI_IGNORE_BOTS", help_heading = "Agent")]
    ignore_bots: bool,

    /// Keep one Claude Code session up per room (the default) instead of
    /// one run per message: what the agent starts in the background keeps
    /// running, and it is woken when that finishes. It answers with
    /// nubi_send, so it needs an MCP server; other harnesses run one run
    /// per message regardless
    #[arg(long, env = "NUBI_LIVE_SESSIONS", default_value_t = true, action = clap::ArgAction::Set, num_args = 0..=1, default_missing_value = "true", help_heading = "Tuning", overrides_with = "no_live_sessions")]
    live_sessions: bool,

    /// One run per message instead of a live session: nothing the agent
    /// starts outlives its turn, and it cannot come back later on its own
    #[arg(
        long,
        env = "NUBI_NO_LIVE_SESSIONS",
        help_heading = "Tuning",
        overrides_with = "live_sessions"
    )]
    no_live_sessions: bool,

    /// Live sessions kept up at once; the one idle longest is stopped first
    #[arg(
        long,
        env = "NUBI_LIVE_MAX",
        default_value = "3",
        help_heading = "Tuning"
    )]
    live_max: usize,

    /// Stop a live session with nothing running after this many idle seconds
    #[arg(
        long,
        env = "NUBI_LIVE_IDLE_SECS",
        default_value = "900",
        help_heading = "Tuning"
    )]
    live_idle_secs: u64,

    /// What one live session serves: `room` (each room and each thread its
    /// own) or `family` (a room and its threads share one)
    #[arg(
        long,
        env = "NUBI_LIVE_SCOPE",
        value_enum,
        default_value_t = LiveScope::Room,
        help_heading = "Tuning"
    )]
    live_scope: LiveScope,

    /// Hold a turn a person started to sending within its room's family
    /// (the room, its parent, their threads). Off by default: the agent may
    /// send anywhere; only what wakes it is guarded
    #[arg(long, env = "NUBI_SEND_LOCK", default_value_t = false, action = clap::ArgAction::Set, num_args = 0..=1, default_missing_value = "true", help_heading = "Tuning")]
    send_lock: bool,

    /// POSTed every minute while the agent is working, for a launcher that
    /// stops idle agents
    #[arg(long, env = "NUBI_HEARTBEAT_URL", help_heading = "Agent")]
    heartbeat_url: Option<String>,

    /// Bearer token sent with --heartbeat-url
    #[arg(
        long,
        env = "NUBI_HEARTBEAT_TOKEN",
        hide_env_values = true,
        help_heading = "Agent"
    )]
    heartbeat_token: Option<String>,

    /// Model for the CLI. Unset means the CLI's own default
    #[arg(long, env = "NUBI_LLM_MODEL", help_heading = "Model")]
    model: Option<String>,

    /// Model the CLI falls back to when the primary one is overloaded
    /// (Claude Code only)
    #[arg(long, env = "NUBI_LLM_FALLBACK_MODEL", help_heading = "Model")]
    fallback_model: Option<String>,

    /// Reasoning effort: low, medium, high, xhigh or max. Defaults to
    /// medium with Claude Code; other CLIs get none. Effort decides how many
    /// model calls a turn takes, and every call reads the whole conversation
    /// again, so it is the main cost lever: raise it for agents that write
    /// or review code, lower it for ones that coordinate
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
        default_value = "50",
        help_heading = "Tuning"
    )]
    compact_at_pct: u8,

    /// The context window, in tokens, that compaction is measured against
    /// (Claude Code only). Unset: looked up from the Models API at startup,
    /// capped at 200k. Explicit N: used as-is, uncapped. 0: compaction off.
    #[arg(long, env = "NUBI_COMPACT_WINDOW", help_heading = "Tuning")]
    compact_window: Option<u32>,

    /// Keep Claude Code's tool search on behind --llm-base-url, so tool
    /// definitions load when needed instead of with every model call. Only
    /// for a gateway that passes `tool_reference` blocks through unchanged
    /// (the Anthropic API does; a Kiro gateway does not, and the session
    /// gets stuck)
    #[arg(long, env = "NUBI_TOOL_SEARCH", help_heading = "Tuning")]
    tool_search: bool,

    /// Kill a run that takes longer than this many seconds and tell the room
    #[arg(
        long,
        env = "NUBI_TURN_TIMEOUT_SECS",
        default_value = "900",
        help_heading = "Tuning"
    )]
    turn_timeout_secs: u64,

    /// How many turns may run at once across all rooms. 1 answers messages
    /// strictly in arrival order, each turn seeing everything the agent did
    /// before it; more answers faster, but a turn may not know what another
    /// one running beside it is doing
    #[arg(
        long,
        env = "NUBI_MAX_PARALLEL_TURNS",
        default_value = "1",
        help_heading = "Tuning"
    )]
    max_parallel_turns: usize,

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

// `nubi-agent hook ...`: run by the coding CLI, not by people. Kept out of
// `Args` so it never needs a token or touches the engine.
#[derive(Parser)]
#[command(
    name = "nubi-agent hook",
    bin_name = "nubi-agent hook",
    about = "Hooks the coding CLI runs (registered by nubi-agent itself)"
)]
struct HookArgs {
    #[command(subcommand)]
    event: HookEvent,
}

#[derive(clap::Subcommand)]
enum HookEvent {
    /// Claude Code's PreToolUse hook: refuse a nubi_send to a room outside
    /// the turn's scope or to a room another turn just sent to (exit 2,
    /// the reason on stderr)
    PreToolUse {
        /// The scope file nubi-agent writes before each turn
        #[arg(long)]
        scope: std::path::PathBuf,
        /// Shared dedup file for cross-turn destination dedup
        #[arg(long)]
        dedup: Option<std::path::PathBuf>,
    },
}

fn run_hook(argv: Vec<std::ffi::OsString>) -> i32 {
    let args = HookArgs::parse_from(argv);
    match args.event {
        HookEvent::PreToolUse { scope, dedup } => {
            let (code, msg) = nubi_agent::send_guard::pre_tool_use(
                &scope,
                dedup.as_deref(),
                std::io::stdin().lock(),
            );
            if let Some(msg) = msg {
                eprintln!("{msg}");
            }
            code
        }
    }
}

const COMPACT_WINDOW_FALLBACK: u32 = 200_000;
// Temporary until WS-M re-measure; explicit env bypasses.
const COMPACT_WINDOW_CAP: u32 = 200_000;

async fn lookup_model_window(base_url: &str, model: &str, api_key: Option<&str>) -> Option<u32> {
    let url = format!("{}/v1/models/{}", base_url.trim_end_matches('/'), model);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .ok()?;
    let mut req = client.get(&url).header("anthropic-version", "2023-06-01");
    if let Some(key) = api_key {
        req = req.header("x-api-key", key);
    }
    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        eprintln!(
            "[nubi-agent] Models API {} returned {}",
            model,
            resp.status()
        );
        return None;
    }
    let body: serde_json::Value = resp.json().await.ok()?;
    body["max_input_tokens"].as_u64().map(|n| n as u32)
}

async fn resolve_compact_window(args: &Args) -> Option<u32> {
    match args.compact_window {
        Some(0) => return None,
        Some(n) => return Some(n),
        None => {}
    }
    if !matches!(args.harness, HarnessKind::Claude) {
        return None;
    }
    let model = match args.model.as_deref() {
        Some(m) => m,
        None => {
            eprintln!(
                "[nubi-agent] compact window: {COMPACT_WINDOW_FALLBACK} (no --model, using fallback)"
            );
            return Some(COMPACT_WINDOW_FALLBACK);
        }
    };
    let base = args
        .llm_base_url
        .as_deref()
        .unwrap_or("https://api.anthropic.com");
    let api_key = args.llm_key.as_deref();
    let primary = lookup_model_window(base, model, api_key).await;
    let window = if let Some(ref fb) = args.fallback_model {
        let fb_window = lookup_model_window(base, fb, api_key).await;
        match (primary, fb_window) {
            (Some(p), Some(f)) => Some(p.min(f)),
            (Some(p), None) => Some(p),
            (None, Some(f)) => Some(f),
            (None, None) => None,
        }
    } else {
        primary
    };
    let w = window.unwrap_or(COMPACT_WINDOW_FALLBACK);
    let capped = w.min(COMPACT_WINDOW_CAP);
    eprintln!("[nubi-agent] compact window: {capped} (api: {w}, cap: {COMPACT_WINDOW_CAP})");
    Some(capped)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    nubi_agent::hardening::deny_same_user_inspection();
    let argv: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if argv.get(1).is_some_and(|a| a == "hook") {
        std::process::exit(run_hook(argv[1..].to_vec()));
    }
    let args = Args::parse();

    nubi_agent::telemetry::init_telemetry("nubi-agent");

    let Some(token) = args.token.clone().filter(|t| !t.trim().is_empty()) else {
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
    let compact_window = resolve_compact_window(&args).await;
    let mcp_url = Some(args.mcp_url.unwrap_or_else(|| default_mcp_url(&args.url)));

    let llm_provider = args
        .llm_provider
        .clone()
        .unwrap_or_else(|| infer_provider(args.llm_base_url.as_deref(), args.harness));
    // Claude Code gets an effort level by default; everything else about the
    // model is the CLI's own unless set, so a logged-in CLI just works.
    let effort = match args.harness {
        HarnessKind::Claude => Some(args.effort.unwrap_or_else(|| "medium".to_string())),
        _ => args.effort,
    };

    let config = AgentConfig {
        agent_id,
        rooms: args.rooms,
        system_prompt: args.system_prompt,
        skip_permissions: args.skip_permissions,
        pass_env: args.pass_env,
        answer_since_ms: args.answer_since_ms,
        answer_since_all_rooms: args.answer_since_all_rooms,
        ignore_bots: args.ignore_bots,
        live_sessions: args.live_sessions && !args.no_live_sessions,
        live_max: args.live_max,
        live_idle_secs: args.live_idle_secs,
        live_scope: args.live_scope,
        send_lock: args.send_lock,
        heartbeat_url: args.heartbeat_url,
        heartbeat_token: args.heartbeat_token,
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
        compact_window,
        tool_search: args.tool_search,
        turn_timeout_secs: args.turn_timeout_secs,
        max_parallel_turns: args.max_parallel_turns,
        harness: args.harness,
        mandate_strict: args.mandate_strict,
        no_local_skills: args.no_local_skills,
    };

    let mut agent = Agent::connect(nubi, config).await;
    let result = tokio::select! {
        r = agent.run() => r,
        _ = shutdown_signal() => {
            eprintln!("[nubi-agent] Received shutdown signal");
            Ok(())
        }
    };
    nubi_agent::telemetry::shutdown_telemetry();
    result
}

async fn shutdown_signal() {
    use tokio::signal;
    let ctrl_c = signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to register SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {}
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        ctrl_c.await.ok();
    }
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
    use super::{default_mcp_url, infer_provider, lookup_model_window, resolve_compact_window};
    use super::{Args, COMPACT_WINDOW_CAP, COMPACT_WINDOW_FALLBACK, HOSTED_ENGINE};
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
        assert!(args.live_sessions && !args.no_live_sessions);
    }

    /// Live sessions are on unless asked off, either way round.
    #[test]
    fn live_sessions_are_on_by_default_and_can_be_turned_off() {
        let live = |argv: &[&str]| {
            let a = Args::try_parse_from(["nubi-agent", "--token", "nubi_x"].iter().chain(argv))
                .unwrap();
            a.live_sessions && !a.no_live_sessions
        };
        assert!(live(&[]));
        assert!(live(&["--live-sessions"]));
        assert!(!live(&["--no-live-sessions"]));
        assert!(!live(&["--live-sessions=false"]));
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

    use std::collections::HashMap;
    use std::sync::Arc;

    struct MockModelsConfig {
        windows: HashMap<String, u64>,
        require_auth: bool,
    }

    async fn mock_models_api(max_input: Option<u64>) -> String {
        let mut windows = HashMap::new();
        if let Some(n) = max_input {
            windows.insert("*".to_string(), n);
        }
        mock_models_api_ext(windows, false).await
    }

    async fn mock_models_api_ext(windows: HashMap<String, u64>, require_auth: bool) -> String {
        use axum::{
            extract::{Path, State},
            http::{HeaderMap, StatusCode},
            response::IntoResponse,
            routing::get,
            Json, Router,
        };

        let config = Arc::new(MockModelsConfig {
            windows,
            require_auth,
        });

        async fn handler(
            State(config): State<Arc<MockModelsConfig>>,
            Path(model_id): Path<String>,
            headers: HeaderMap,
        ) -> impl IntoResponse {
            if config.require_auth
                && !headers.contains_key("x-api-key")
                && !headers.contains_key("authorization")
            {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(serde_json::json!({"error": "unauthorized"})),
                )
                    .into_response();
            }
            let window = config
                .windows
                .get(&model_id)
                .or_else(|| config.windows.get("*"));
            match window {
                Some(&n) => Json(serde_json::json!({
                    "id": model_id,
                    "max_input_tokens": n,
                }))
                .into_response(),
                None => Json(serde_json::json!({"id": model_id})).into_response(),
            }
        }

        let app = Router::new()
            .route("/v1/models/{model_id}", get(handler))
            .with_state(config);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn lookup_model_window_returns_max_input_tokens() {
        let url = mock_models_api(Some(180_000)).await;
        let w = lookup_model_window(&url, "claude-sonnet-4-20250514", None).await;
        assert_eq!(w, Some(180_000));
    }

    #[tokio::test]
    async fn lookup_model_window_returns_none_when_field_missing() {
        let url = mock_models_api(None).await;
        let w = lookup_model_window(&url, "claude-sonnet-4-20250514", None).await;
        assert_eq!(w, None);
    }

    #[tokio::test]
    async fn lookup_model_window_returns_none_on_connection_error() {
        let w = lookup_model_window("http://127.0.0.1:1", "x", None).await;
        assert_eq!(w, None);
    }

    #[tokio::test]
    async fn lookup_model_window_sends_auth_header() {
        let url = mock_models_api_ext(HashMap::from([("*".to_string(), 180_000)]), true).await;
        let without = lookup_model_window(&url, "claude-sonnet-4-20250514", None).await;
        assert_eq!(without, None, "should fail without api key");
        let with = lookup_model_window(&url, "claude-sonnet-4-20250514", Some("sk-test")).await;
        assert_eq!(with, Some(180_000), "should succeed with api key");
    }

    #[tokio::test]
    async fn resolve_uses_explicit_value_when_set() {
        let args = Args::try_parse_from([
            "nubi-agent",
            "--token",
            "nubi_x",
            "--compact-window",
            "150000",
        ])
        .unwrap();
        assert_eq!(resolve_compact_window(&args).await, Some(150_000));
    }

    #[tokio::test]
    async fn resolve_zero_disables_compaction() {
        let args =
            Args::try_parse_from(["nubi-agent", "--token", "nubi_x", "--compact-window", "0"])
                .unwrap();
        assert_eq!(resolve_compact_window(&args).await, None);
    }

    #[tokio::test]
    async fn resolve_calls_api_when_unset() {
        let url = mock_models_api(Some(180_000)).await;
        let args = Args::try_parse_from([
            "nubi-agent",
            "--token",
            "nubi_x",
            "--llm-base-url",
            &url,
            "--model",
            "claude-sonnet-4-20250514",
        ])
        .unwrap();
        assert_eq!(resolve_compact_window(&args).await, Some(180_000));
    }

    #[tokio::test]
    async fn resolve_uses_fallback_when_no_model() {
        let args = Args::try_parse_from(["nubi-agent", "--token", "nubi_x"]).unwrap();
        assert_eq!(
            resolve_compact_window(&args).await,
            Some(COMPACT_WINDOW_FALLBACK)
        );
    }

    #[tokio::test]
    async fn resolve_falls_back_when_api_unreachable() {
        let args = Args::try_parse_from([
            "nubi-agent",
            "--token",
            "nubi_x",
            "--llm-base-url",
            "http://127.0.0.1:1",
            "--model",
            "claude-sonnet-4-20250514",
        ])
        .unwrap();
        assert_eq!(
            resolve_compact_window(&args).await,
            Some(COMPACT_WINDOW_FALLBACK)
        );
    }

    #[tokio::test]
    async fn resolve_caps_large_api_window() {
        let url = mock_models_api(Some(1_000_000)).await;
        let args = Args::try_parse_from([
            "nubi-agent",
            "--token",
            "nubi_x",
            "--llm-base-url",
            &url,
            "--model",
            "claude-sonnet-4-20250514",
        ])
        .unwrap();
        assert_eq!(
            resolve_compact_window(&args).await,
            Some(COMPACT_WINDOW_CAP)
        );
    }

    #[tokio::test]
    async fn resolve_api_below_cap_passes_through() {
        let url = mock_models_api(Some(100_000)).await;
        let args = Args::try_parse_from([
            "nubi-agent",
            "--token",
            "nubi_x",
            "--llm-base-url",
            &url,
            "--model",
            "claude-sonnet-4-20250514",
        ])
        .unwrap();
        assert_eq!(resolve_compact_window(&args).await, Some(100_000));
    }

    #[tokio::test]
    async fn resolve_explicit_bypasses_cap() {
        let args = Args::try_parse_from([
            "nubi-agent",
            "--token",
            "nubi_x",
            "--compact-window",
            "1000000",
        ])
        .unwrap();
        assert_eq!(resolve_compact_window(&args).await, Some(1_000_000));
    }

    #[tokio::test]
    async fn resolve_uses_min_of_primary_and_fallback_model() {
        let url = mock_models_api_ext(
            HashMap::from([
                ("claude-sonnet-4-20250514".to_string(), 180_000),
                ("claude-haiku-4-5-20241022".to_string(), 150_000),
            ]),
            false,
        )
        .await;
        let args = Args::try_parse_from([
            "nubi-agent",
            "--token",
            "nubi_x",
            "--llm-base-url",
            &url,
            "--model",
            "claude-sonnet-4-20250514",
            "--fallback-model",
            "claude-haiku-4-5-20241022",
        ])
        .unwrap();
        assert_eq!(resolve_compact_window(&args).await, Some(150_000));
    }

    #[tokio::test]
    async fn resolve_returns_none_for_non_claude() {
        let args = Args::try_parse_from(["nubi-agent", "--token", "nubi_x", "--harness", "codex"])
            .unwrap();
        assert_eq!(resolve_compact_window(&args).await, None);
    }
}
