use std::path::PathBuf;

use super::{pass_mcp_token, write_private, Harness, Invocation, Parsed, Settings, MCP_TOKEN_ENV};
use crate::runtime::Session;

/// Google Gemini CLI: `gemini -p`.
pub struct Gemini;

impl Gemini {
    fn system_prompt_path(settings: &Settings) -> PathBuf {
        settings.state_dir().join("gemini-system.md")
    }

    fn settings_path(settings: &Settings) -> PathBuf {
        settings.state_dir().join("gemini-settings.json")
    }
}

impl Harness for Gemini {
    fn binary(&self) -> &'static str {
        "gemini"
    }

    fn env_prefixes(&self) -> Vec<&'static str> {
        vec!["GEMINI_", "GOOGLE_"]
    }

    fn install_hint(&self) -> &'static str {
        "npm install -g @google/gemini-cli"
    }

    fn memory_file(&self) -> &'static str {
        "GEMINI.md"
    }

    fn prepare(&self, settings: &Settings, system_prompt: &str) -> anyhow::Result<()> {
        write_private(&Self::system_prompt_path(settings), system_prompt)?;

        // Loaded as the system settings tier, so the user's own
        // ~/.gemini/settings.json is left alone. The token is interpolated
        // from the env at run time and never lands in this file.
        let mut config = serde_json::json!({});
        if let Some(ref mcp) = settings.mcp {
            config["mcpServers"] = serde_json::json!({
                "nubi": {
                    "httpUrl": mcp.url,
                    "headers": { "Authorization": format!("Bearer ${{{MCP_TOKEN_ENV}}}") },
                    "trust": true
                }
            });
        }
        write_private(
            &Self::settings_path(settings),
            &serde_json::to_string_pretty(&config)?,
        )
    }

    fn invocation(
        &self,
        settings: &Settings,
        _system_prompt: &str,
        prompt: &str,
        _session: Option<&Session>,
    ) -> Invocation {
        let mut inv = Invocation::default();
        inv.arg("--output-format").arg("json");
        if settings.skip_permissions {
            inv.arg("--approval-mode").arg("yolo");
        }
        if let Some(ref model) = settings.model {
            inv.arg("--model").arg(model);
        }
        inv.arg("-p").arg(prompt);

        // Replaces Gemini's built-in system prompt, as --system-prompt does
        // for Claude.
        inv.env(
            "GEMINI_SYSTEM_MD",
            Self::system_prompt_path(settings).to_string_lossy(),
        )
        .env(
            "GEMINI_CLI_SYSTEM_SETTINGS_PATH",
            Self::settings_path(settings).to_string_lossy(),
        )
        // Headless runs cannot answer the trust-this-folder prompt.
        .env("GEMINI_CLI_TRUST_WORKSPACE", "true");
        pass_mcp_token(settings, &mut inv);

        if let Some(ref url) = settings.llm_base_url {
            inv.env("GOOGLE_GEMINI_BASE_URL", url);
        }
        if let Some(ref key) = settings.llm_api_key {
            inv.env("GEMINI_API_KEY", key);
        }
        inv
    }

    fn parse_output(&self, stdout: &str) -> anyhow::Result<Parsed> {
        let value: serde_json::Value = serde_json::from_str(stdout.trim())
            .map_err(|e| anyhow::anyhow!("gemini printed something other than JSON: {e}"))?;
        if let Some(err) = value.get("error").filter(|e| !e.is_null()) {
            anyhow::bail!("gemini reported an error: {err}");
        }
        Ok(Parsed::text(value["response"].as_str().unwrap_or_default()))
    }
}
