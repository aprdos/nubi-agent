use std::path::PathBuf;

use super::{write_private, Harness, Invocation, Settings};

/// Anthropic Claude Code: `claude -p`.
pub struct Claude;

impl Claude {
    fn mcp_config_path(settings: &Settings) -> PathBuf {
        settings.state_dir().join("claude-mcp.json")
    }
}

impl Harness for Claude {
    fn binary(&self) -> &'static str {
        "claude"
    }

    fn install_hint(&self) -> &'static str {
        "npm install -g @anthropic-ai/claude-code"
    }

    fn memory_file(&self) -> &'static str {
        "CLAUDE.md"
    }

    fn prepare(&self, settings: &Settings, _system_prompt: &str) -> anyhow::Result<()> {
        if let Some(ref mcp) = settings.mcp {
            let config = serde_json::json!({
                "mcpServers": {
                    "nubi": {
                        "type": "http",
                        "url": mcp.url,
                        "headers": { "Authorization": format!("Bearer {}", mcp.token) }
                    }
                }
            });
            write_private(
                &Self::mcp_config_path(settings),
                &serde_json::to_string_pretty(&config)?,
            )?;
        }
        Ok(())
    }

    fn invocation(&self, settings: &Settings, system_prompt: &str, prompt: &str) -> Invocation {
        let mut inv = Invocation::default();
        inv.arg("-p").arg(prompt);
        if settings.skip_permissions {
            inv.arg("--dangerously-skip-permissions");
        }
        if let Some(ref model) = settings.model {
            inv.arg("--model").arg(model);
        }
        inv.arg("--system-prompt").arg(system_prompt);
        if settings.mcp.is_some() {
            inv.arg("--mcp-config")
                .arg(Self::mcp_config_path(settings).to_string_lossy())
                // Headless runs cannot answer a permission prompt, so without
                // this every nubi tool call is denied unless the agent also
                // runs with --skip-permissions. The `=` form keeps the
                // variadic flag from swallowing the next argument.
                .arg("--allowedTools=mcp__nubi");
        }
        if let Some(ref url) = settings.llm_base_url {
            inv.env("ANTHROPIC_BASE_URL", url);
        }
        if let Some(ref key) = settings.llm_api_key {
            inv.env("ANTHROPIC_API_KEY", key);
        }
        inv
    }
}
