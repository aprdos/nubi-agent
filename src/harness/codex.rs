use super::{pass_mcp_token, toml_string, Harness, Invocation, Settings, MCP_TOKEN_ENV};

/// OpenAI Codex CLI: `codex exec`.
pub struct Codex;

impl Harness for Codex {
    fn binary(&self) -> &'static str {
        "codex"
    }

    fn install_hint(&self) -> &'static str {
        "npm install -g @openai/codex"
    }

    fn memory_file(&self) -> &'static str {
        "AGENTS.md"
    }

    fn invocation(&self, settings: &Settings, system_prompt: &str, prompt: &str) -> Invocation {
        let mut inv = Invocation::default();
        // The answer goes to a file of its own; stdout also carries progress.
        let out = settings
            .state_dir()
            .join(format!("codex-{}.txt", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(settings.state_dir());

        inv.arg("exec")
            // The workspace is a plain directory, not a git checkout.
            .arg("--skip-git-repo-check")
            .arg("--ephemeral")
            .arg("--output-last-message")
            .arg(out.to_string_lossy());
        inv.output_file = Some(out);

        // Added on top of Codex's own instructions, unlike AGENTS.md which
        // stays the agent's memory.
        inv.arg("-c").arg(format!(
            "developer_instructions={}",
            toml_string(system_prompt)
        ));

        if let Some(ref mcp) = settings.mcp {
            inv.arg("-c")
                .arg(format!("mcp_servers.nubi.url={}", toml_string(&mcp.url)))
                .arg("-c")
                .arg(format!(
                    "mcp_servers.nubi.bearer_token_env_var={}",
                    toml_string(MCP_TOKEN_ENV)
                ))
                // exec has nobody to ask, and without this it cancels every
                // nubi tool call as if the user had said no.
                .arg("-c")
                .arg("mcp_servers.nubi.default_tools_approval_mode=\"approve\"")
                // Otherwise the first turn can start before the server's tools
                // are listed, and the model never sees them.
                .arg("-c")
                .arg("mcp_servers.nubi.required=true");
            pass_mcp_token(settings, &mut inv);
        }

        if settings.skip_permissions {
            inv.arg("--dangerously-bypass-approvals-and-sandbox");
        }
        if let Some(ref model) = settings.model {
            inv.arg("--model").arg(model);
        }
        if let Some(ref url) = settings.llm_base_url {
            inv.arg("-c")
                .arg(format!("openai_base_url={}", toml_string(url)));
        }
        if let Some(ref key) = settings.llm_api_key {
            inv.env("CODEX_API_KEY", key);
        }

        // Read from stdin ("-") so a long room context never hits argv limits.
        inv.arg("-");
        inv.stdin = Some(prompt.to_string());
        inv
    }
}
