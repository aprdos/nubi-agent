//! Run the agent's prompt against a set of recorded room situations and have
//! a model grade each reply against a rubric.
//!
//!   cargo run --example eval -- docs/eval/cases.jsonl
//!
//! One case per line:
//!
//!   {"id": "merge-question", "room_id": "pr-review",
//!    "context": [{"sender": "alice", "text": "PR #50 is up"}],
//!    "trigger": {"sender": "bob", "text": "@bot can it be merged?"},
//!    "rubric": "Says it will check or checks before claiming; no bare yes/no guess."}
//!
//! The harness, model and effort come from the same env vars as the smoke
//! example (NUBI_HARNESS, NUBI_LLM_MODEL, NUBI_LLM_EFFORT, NUBI_LLM_BASE_URL,
//! NUBI_LLM_KEY). Without NUBI_MCP_URL and NUBI_TOKEN the agent has no nubi
//! tools, so rubrics should judge the text, not tool use. Each run costs
//! real model calls: two per case.

use clap::ValueEnum;
use nubi_agent::harness::{CliRuntime, HarnessKind, McpServer};
use nubi_agent::{AgentRuntime, Trigger, TurnInput};
use serde::Deserialize;

#[derive(Deserialize)]
struct Case {
    id: String,
    #[serde(default = "default_room")]
    room_id: String,
    #[serde(default)]
    context: Vec<Line>,
    trigger: Line,
    rubric: String,
}

#[derive(Deserialize)]
struct Line {
    sender: String,
    text: String,
}

fn default_room() -> String {
    "general".into()
}

fn runtime(kind: HarnessKind, agent_id: &str) -> CliRuntime {
    let workspace = std::env::temp_dir().join(format!("nubi-agent-eval-{agent_id}"));
    let _ = std::fs::create_dir_all(&workspace);
    let mcp = match (std::env::var("NUBI_MCP_URL"), std::env::var("NUBI_TOKEN")) {
        (Ok(url), Ok(token)) => Some(McpServer { url, token }),
        _ => None,
    };
    CliRuntime::new(kind, agent_id)
        .with_workspace(Some(workspace))
        .with_model(std::env::var("NUBI_LLM_MODEL").ok())
        .with_effort(std::env::var("NUBI_LLM_EFFORT").ok())
        .with_llm_base_url(std::env::var("NUBI_LLM_BASE_URL").ok())
        .with_llm_api_key(std::env::var("NUBI_LLM_KEY").ok())
        .with_mcp(mcp)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "docs/eval/cases.jsonl".to_string());
    let kind = std::env::var("NUBI_HARNESS").unwrap_or_else(|_| "claude".into());
    let kind = HarnessKind::from_str(&kind, true).map_err(anyhow::Error::msg)?;

    let cases: Vec<Case> = std::fs::read_to_string(&path)?
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    eprintln!("[eval] {} case(s) from {path} on {kind:?}", cases.len());

    let agent = runtime(kind, "eval-bot");
    agent.check_available().await?;
    // The judge runs with no nubi persona at all, just the grading prompt.
    let judge = runtime(kind, "eval-judge").with_system_prompt(Some(
        "You grade chat replies against a rubric. Be strict and literal.".into(),
    ));

    let mut passed = 0;
    let mut cost = 0.0;
    for case in &cases {
        let now = nubi_agent::harness::now_ms();
        let context = case
            .context
            .iter()
            .enumerate()
            .map(|(i, l)| nubi_agent::BufferedMessage::plain(i as u64 + 1, &l.sender, &l.text, now))
            .collect();
        let turn = TurnInput {
            room_id: case.room_id.clone(),
            triggers: vec![Trigger {
                sender: case.trigger.sender.clone(),
                text: case.trigger.text.clone(),
                index: case.context.len() as u64 + 1,
                expect_reply: None,
                timestamp_ms: now,
                attachments: vec![],
            }],
            context,
            ..Default::default()
        };
        let out = agent.run(&agent.build_prompt(&turn), None).await?;
        cost += out.cost_usd.unwrap_or(0.0);
        let reply = match &out.reply {
            Some(r) if !r.post => "(posted nothing; answered via tools)".to_string(),
            Some(r) => format!(
                "{}\n[mentions: {:?}, expect_reply: {:?}]",
                r.text, r.mentions, r.expect_reply
            ),
            None => out.text.clone(),
        };

        let verdict = judge
            .run(
                &format!(
                    "Rubric:\n{}\n\nThe message being answered, from {}:\n{}\n\nThe reply:\n{}\n\n\
                     Does the reply satisfy the rubric? Answer with PASS or FAIL on the first \
                     line and one sentence of reason on the second.",
                    case.rubric, case.trigger.sender, case.trigger.text, reply
                ),
                None,
            )
            .await?;
        cost += verdict.cost_usd.unwrap_or(0.0);
        let text = verdict.text.trim();
        let ok = text.to_ascii_uppercase().starts_with("PASS");
        if ok {
            passed += 1;
        }
        println!(
            "{} {}\n  reply: {}\n  judge: {}\n",
            if ok { "PASS" } else { "FAIL" },
            case.id,
            reply.replace('\n', "\n         "),
            text.replace('\n', " ")
        );
    }
    println!(
        "{passed}/{} passed, about ${cost:.2} in model calls",
        cases.len()
    );
    Ok(())
}
