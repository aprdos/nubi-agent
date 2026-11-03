use super::{pass_mcp_token, with_instructions, Harness, Invocation, Settings, MCP_TOKEN_ENV};
use crate::runtime::Session;

/// GitHub Copilot CLI: `copilot -p`.
pub struct Copilot;

impl Harness for Copilot {
    fn binary(&self) -> &'static str {
        "copilot"
    }

    fn env_prefixes(&self) -> Vec<&'static str> {
        vec!["COPILOT_", "GH_TOKEN", "GITHUB_TOKEN"]
    }

    fn install_hint(&self) -> &'static str {
        "npm install -g @github/copilot"
    }

    fn memory_file(&self) -> &'static str {
        "AGENTS.md"
    }

    fn invocation(
        &self,
        settings: &Settings,
        system_prompt: &str,
        prompt: &str,
        _session: Option<&Session>,
    ) -> Invocation {
        let mut inv = Invocation::default();
        // -s prints only the agent's response, without usage stats.
        inv.arg("-p")
            .arg(with_instructions(system_prompt, prompt))
            .arg("-s")
            .arg("--no-ask-user");

        if let Some(ref mcp) = settings.mcp {
            // For this session only, on top of ~/.copilot/mcp-config.json.
            let config = serde_json::json!({
                "mcpServers": {
                    "nubi": {
                        "type": "http",
                        "url": mcp.url,
                        "headers": { "Authorization": format!("Bearer ${{{MCP_TOKEN_ENV}}}") },
                        "tools": ["*"]
                    }
                }
            });
            inv.arg(format!("--additional-mcp-config={config}"))
                // The nubi tools are the point; allow them even without
                // --skip-permissions.
                .arg("--allow-tool=nubi");
            pass_mcp_token(settings, &mut inv);
        }
        if settings.skip_permissions {
            inv.arg("--allow-all-tools");
        }
        if let Some(ref model) = settings.model {
            inv.arg(format!("--model={model}"));
        }
        if let Some(ref url) = settings.llm_base_url {
            inv.env("COPILOT_PROVIDER_BASE_URL", url);
        }
        if let Some(ref key) = settings.llm_api_key {
            inv.env("COPILOT_PROVIDER_API_KEY", key);
        }
        inv
    }
}
