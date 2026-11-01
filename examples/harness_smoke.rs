//! Run one harness once, outside nubi, to check an adapter end to end.
//!
//!   cargo run --example harness_smoke -- codex "say hi in three words"
//!
//! No MCP server is wired in, so this only exercises the spawn, the system
//! prompt and output parsing.

use clap::ValueEnum;
use nubi_agent::harness::{CliRuntime, HarnessKind};
use nubi_agent::AgentRuntime;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let kind = args.next().unwrap_or_else(|| "claude".to_string());
    let kind = HarnessKind::from_str(&kind, true).map_err(anyhow::Error::msg)?;
    let message = args.next().unwrap_or_else(|| "say hi in three words".to_string());
    let model = std::env::var("NUBI_LLM_MODEL").ok();
    let base_url = std::env::var("NUBI_LLM_BASE_URL").ok();
    let key = std::env::var("NUBI_LLM_KEY").ok();

    let workspace = std::env::temp_dir().join("nubi-agent-smoke");
    std::fs::create_dir_all(&workspace)?;

    let rt = CliRuntime::new(kind, "smoke-bot")
        .with_workspace(Some(workspace))
        .with_model(model)
        .with_llm_base_url(base_url)
        .with_llm_api_key(key);
    rt.check_available().await?;
    let prompt = rt.build_prompt("dm.alice.smoke-bot", "alice", &message, &[]);
    println!("{}", rt.run(&prompt).await?);
    Ok(())
}
