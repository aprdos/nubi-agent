use std::path::PathBuf;

use super::{pass_mcp_token, write_private, Harness, Invocation, Settings, MCP_TOKEN_ENV};

/// OpenCode: `opencode run`.
pub struct OpenCode;

impl OpenCode {
    fn instructions_path(settings: &Settings) -> PathBuf {
        settings.state_dir().join("opencode-instructions.md")
    }

    /// Inline config merged over the user's own, so nothing global changes.
    fn config(settings: &Settings) -> serde_json::Value {
        let mut config = serde_json::json!({
            // Added to OpenCode's own system prompt.
            "instructions": [Self::instructions_path(settings).to_string_lossy()],
        });
        if let Some(ref mcp) = settings.mcp {
            config["mcp"] = serde_json::json!({
                "nubi": {
                    "type": "remote",
                    "url": mcp.url,
                    "enabled": true,
                    "oauth": false,
                    "headers": { "Authorization": format!("Bearer {{env:{MCP_TOKEN_ENV}}}") }
                }
            });
        }
        // Base URL and key are set per provider, and the provider is the part
        // of the model id before the slash (anthropic/claude-sonnet-4-5).
        let provider = settings
            .model
            .as_deref()
            .and_then(|m| m.split_once('/'))
            .map(|(p, _)| p);
        if let Some(provider) = provider {
            let mut options = serde_json::Map::new();
            if let Some(ref url) = settings.llm_base_url {
                options.insert("baseURL".into(), url.clone().into());
            }
            if let Some(ref key) = settings.llm_api_key {
                options.insert("apiKey".into(), key.clone().into());
            }
            if !options.is_empty() {
                config["provider"] = serde_json::json!({ provider: { "options": options } });
            }
        }
        config
    }
}

impl Harness for OpenCode {
    fn binary(&self) -> &'static str {
        "opencode"
    }

    fn install_hint(&self) -> &'static str {
        "npm install -g opencode-ai"
    }

    fn memory_file(&self) -> &'static str {
        "AGENTS.md"
    }

    fn prepare(&self, settings: &Settings, system_prompt: &str) -> anyhow::Result<()> {
        let has_provider = settings.model.as_deref().is_some_and(|m| m.contains('/'));
        if (settings.llm_base_url.is_some() || settings.llm_api_key.is_some()) && !has_provider {
            anyhow::bail!(
                "opencode sets the LLM base URL and key per provider; pass --model as provider/model"
            );
        }
        write_private(&Self::instructions_path(settings), system_prompt)
    }

    fn invocation(&self, settings: &Settings, _system_prompt: &str, prompt: &str) -> Invocation {
        let mut inv = Invocation::default();
        inv.arg("run").arg("--format").arg("json");
        if settings.skip_permissions {
            inv.arg("--auto");
        }
        if let Some(ref model) = settings.model {
            inv.arg("--model").arg(model);
        }
        inv.env(
            "OPENCODE_CONFIG_CONTENT",
            Self::config(settings).to_string(),
        );
        pass_mcp_token(settings, &mut inv);
        // Read from stdin when it is not a terminal.
        inv.stdin = Some(prompt.to_string());
        inv
    }

    /// JSON lines; the answer is the text of the last `text` event. Earlier
    /// ones are what the model said between tool calls.
    fn parse_output(&self, stdout: &str) -> anyhow::Result<String> {
        let mut answer = None;
        for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
            let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            match event["type"].as_str() {
                Some("text") => answer = event["part"]["text"].as_str().map(str::to_string),
                Some("error") => anyhow::bail!("opencode reported an error: {}", event),
                _ => {}
            }
        }
        Ok(answer.unwrap_or_default())
    }
}
