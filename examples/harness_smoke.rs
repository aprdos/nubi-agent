//! Run one harness once, outside nubi, to check an adapter end to end.
//!
//!   cargo run --example harness_smoke -- codex "say hi in three words"
//!
//! By default no MCP server is wired in, so this only exercises the spawn,
//! the system prompt and output parsing. Set NUBI_MCP_URL and NUBI_TOKEN to
//! hand the harness the nubi tools as well, and ask for something that needs
//! them:
//!
//!   NUBI_MCP_URL=http://localhost:8089/mcp NUBI_TOKEN=nubi_xxx \
//!     cargo run --example harness_smoke -- codex "use nubi_list_rooms: how many rooms?"

use clap::ValueEnum;
use nubi_agent::harness::{CliRuntime, HarnessKind, McpServer};
use nubi_agent::{AgentRuntime, Trigger, TurnInput};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let kind = args.next().unwrap_or_else(|| "claude".to_string());
    let kind = HarnessKind::from_str(&kind, true).map_err(anyhow::Error::msg)?;
    let message = args
        .next()
        .unwrap_or_else(|| "say hi in three words".to_string());
    let model = std::env::var("NUBI_LLM_MODEL").ok();
    let base_url = std::env::var("NUBI_LLM_BASE_URL").ok();
    let key = std::env::var("NUBI_LLM_KEY").ok();
    let mcp = match (std::env::var("NUBI_MCP_URL"), std::env::var("NUBI_TOKEN")) {
        (Ok(url), Ok(token)) => Some(McpServer { url, token }),
        _ => None,
    };
    eprintln!(
        "[smoke] {:?}, nubi tools {}",
        kind,
        if mcp.is_some() { "on" } else { "off" }
    );

    let workspace = std::env::temp_dir().join("nubi-agent-smoke");
    std::fs::create_dir_all(&workspace)?;

    let rt = CliRuntime::new(kind, "smoke-bot")
        .with_workspace(Some(workspace))
        .with_model(model)
        .with_effort(std::env::var("NUBI_LLM_EFFORT").ok())
        .with_llm_base_url(base_url)
        .with_llm_api_key(key)
        .with_mcp(mcp);
    let rt = match std::env::var("NUBI_TURN_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
    {
        Some(secs) => rt.with_turn_timeout(std::time::Duration::from_secs(secs)),
        None => rt,
    };
    rt.check_available().await?;
    if std::env::var_os("NUBI_SMOKE_PRINT_SYSTEM_PROMPT").is_some() {
        eprintln!(
            "[smoke] system prompt:\n{}\n[smoke] end of system prompt",
            rt.build_system_prompt()
        );
    }
    let turn = TurnInput {
        room_id: "dm.alice.smoke-bot".into(),
        triggers: vec![Trigger {
            sender: "alice".into(),
            text: message,
            index: 1,
            expect_reply: None,
            cc: Vec::new(),
            timestamp_ms: 0,
            attachments: vec![],
        }],
        ..Default::default()
    };
    // NUBI_SMOKE_SESSION_ID starts (or, with NUBI_SMOKE_RESUME=1, continues)
    // a harness conversation, to check that sessions survive between runs.
    let session = std::env::var("NUBI_SMOKE_SESSION_ID")
        .ok()
        .map(|id| nubi_agent::Session {
            id,
            resume: std::env::var_os("NUBI_SMOKE_RESUME").is_some(),
        });
    let prompt = rt.build_prompt(&turn);
    let out = rt.run(&prompt, session.as_ref()).await?;
    if out.is_error {
        anyhow::bail!("run failed: {}", out.error.unwrap_or_default());
    }
    println!("{}", out.text);
    if let Some(reply) = out.reply {
        eprintln!(
            "[smoke] mentions={:?} cc={:?} post={}",
            reply.mentions, reply.cc, reply.post
        );
    }
    eprintln!(
        "[smoke] turns={:?} cost_usd={:?} session={:?}",
        out.num_turns, out.cost_usd, out.session_id
    );
    Ok(())
}
