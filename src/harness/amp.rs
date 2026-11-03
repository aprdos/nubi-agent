use super::{
    pass_mcp_token, warn_unsupported, with_instructions, Harness, Invocation, Parsed, Settings,
    MCP_TOKEN_ENV,
};
use crate::runtime::Session;

/// Sourcegraph Amp: `amp -x`.
pub struct Amp;

impl Harness for Amp {
    fn binary(&self) -> &'static str {
        "amp"
    }

    fn env_prefixes(&self) -> Vec<&'static str> {
        vec!["AMP_"]
    }

    fn install_hint(&self) -> &'static str {
        "curl -fsSL https://ampcode.com/install.sh | bash"
    }

    fn memory_file(&self) -> &'static str {
        "AGENTS.md"
    }

    fn prepare(&self, settings: &Settings, _system_prompt: &str) -> anyhow::Result<()> {
        warn_unsupported(self.binary(), "an LLM base URL", &settings.llm_base_url);
        Ok(())
    }

    fn invocation(
        &self,
        settings: &Settings,
        system_prompt: &str,
        prompt: &str,
        _session: Option<&Session>,
    ) -> Invocation {
        let mut inv = Invocation::default();
        if let Some(ref mcp) = settings.mcp {
            // Inline config needs no `amp mcp approve`. The token stays in
            // the env: argv is visible to every local user through ps.
            let config = serde_json::json!({
                "nubi": {
                    "url": mcp.url,
                    "headers": { "Authorization": format!("Bearer ${{{MCP_TOKEN_ENV}}}") }
                }
            });
            inv.arg("--mcp-config").arg(config.to_string());
            pass_mcp_token(settings, &mut inv);
        }
        if settings.skip_permissions {
            inv.arg("--dangerously-allow-all");
        }
        // Amp picks models itself; --model selects its mode (low, medium,
        // high, ultra).
        if let Some(ref mode) = settings.model {
            inv.arg("--mode").arg(mode);
        }
        if let Some(ref key) = settings.llm_api_key {
            inv.env("AMP_API_KEY", key);
        }
        inv.arg("--stream-json")
            .arg("-x")
            .arg(with_instructions(system_prompt, prompt));
        inv
    }

    /// JSON lines ending in a `result` event that holds the final message.
    fn parse_output(&self, stdout: &str) -> anyhow::Result<Parsed> {
        for line in stdout.lines().rev().filter(|l| !l.trim().is_empty()) {
            let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if event["type"] == "result" {
                if event["is_error"].as_bool().unwrap_or(false) {
                    anyhow::bail!("amp reported an error: {}", event);
                }
                return Ok(Parsed::text(event["result"].as_str().unwrap_or_default()));
            }
        }
        anyhow::bail!("amp finished without a result event")
    }
}
