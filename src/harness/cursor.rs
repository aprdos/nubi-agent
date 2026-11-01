use super::{
    pass_mcp_token, warn_unsupported, with_instructions, write_private, Harness, Invocation,
    Settings, MCP_TOKEN_ENV,
};

/// Cursor CLI: `cursor-agent -p`.
pub struct Cursor;

impl Harness for Cursor {
    fn binary(&self) -> &'static str {
        "cursor-agent"
    }

    fn install_hint(&self) -> &'static str {
        "curl https://cursor.com/install -fsS | bash"
    }

    fn memory_file(&self) -> &'static str {
        "AGENTS.md"
    }

    fn prepare(&self, settings: &Settings, _system_prompt: &str) -> anyhow::Result<()> {
        warn_unsupported(self.binary(), "an LLM base URL", &settings.llm_base_url);

        let Some(ref mcp) = settings.mcp else {
            return Ok(());
        };
        // Cursor only reads MCP servers from config files, never from a flag,
        // so the server goes into the workspace's .cursor/. The token stays
        // in the env.
        let Some(ref workspace) = settings.workspace else {
            anyhow::bail!("cursor-agent needs a workspace directory to load the nubi MCP server");
        };
        let dir = workspace.join(".cursor");
        let config = serde_json::json!({
            "mcpServers": {
                "nubi": {
                    "url": mcp.url,
                    "headers": { "Authorization": format!("Bearer ${{env:{MCP_TOKEN_ENV}}}") }
                }
            }
        });
        write_private(
            &dir.join("mcp.json"),
            &serde_json::to_string_pretty(&config)?,
        )?;
        // Allow the nubi tools even without --force; they are the point.
        let permissions = serde_json::json!({ "permissions": { "allow": ["Mcp(nubi:*)"] } });
        write_private(
            &dir.join("cli.json"),
            &serde_json::to_string_pretty(&permissions)?,
        )
    }

    fn invocation(&self, settings: &Settings, system_prompt: &str, prompt: &str) -> Invocation {
        let mut inv = Invocation::default();
        // `text` prints only the final message, after the last tool call.
        inv.arg("-p")
            .arg("--output-format")
            .arg("text")
            .arg("--trust")
            .arg("--approve-mcps");
        if settings.skip_permissions {
            inv.arg("--force");
        }
        if let Some(ref model) = settings.model {
            inv.arg("--model").arg(model);
        }
        if let Some(ref key) = settings.llm_api_key {
            inv.env("CURSOR_API_KEY", key);
        }
        pass_mcp_token(settings, &mut inv);
        inv.arg(with_instructions(system_prompt, prompt));
        inv
    }
}
