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
use nubi_agent::AgentRuntime;

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
        .with_llm_base_url(base_url)
        .with_llm_api_key(key)
        .with_mcp(mcp);
    rt.check_available().await?;
    let prompt = rt.build_prompt("dm.alice.smoke-bot", "alice", &message, &[]);
    println!("{}", rt.run(&prompt).await?.text);
    Ok(())
}
