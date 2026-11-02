use std::path::{Path, PathBuf};

use super::{write_private, Harness, Invocation, Parsed, Settings, REPLY_SCHEMA};
use crate::runtime::{Session, TokenUsage};

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

    fn supports_sessions(&self) -> bool {
        true
    }

    /// Claude Code loads project skills from `.claude/skills/` in its cwd.
    fn skills_dir(&self, workspace: &Path) -> Option<PathBuf> {
        Some(workspace.join(".claude").join("skills"))
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

    fn invocation(
        &self,
        settings: &Settings,
        system_prompt: &str,
        prompt: &str,
        session: Option<&Session>,
    ) -> Invocation {
        let mut inv = Invocation::default();
        inv.arg("-p").arg(prompt);
        // JSON carries is_error, num_turns, session_id, cost and the
        // structured reply; plain text carries none of them.
        inv.arg("--output-format").arg("json");
        inv.arg("--json-schema").arg(REPLY_SCHEMA.to_string());
        if settings.skip_permissions {
            inv.arg("--dangerously-skip-permissions");
        }
        if let Some(ref model) = settings.model {
            inv.arg("--model").arg(model);
        }
        if let Some(ref model) = settings.fallback_model {
            inv.arg("--fallback-model").arg(model);
        }
        if let Some(ref effort) = settings.effort {
            inv.arg("--effort").arg(effort);
        }
        // Appended, not replacing: Claude Code's own prompt carries its tool
        // guidance for Bash/Edit/Read, which the agent uses as much as nubi.
        inv.arg("--append-system-prompt").arg(system_prompt);
        match session {
            Some(s) if s.resume => {
                inv.arg("--resume").arg(&s.id);
            }
            Some(s) => {
                inv.arg("--session-id").arg(&s.id);
            }
            None => {}
        }
        if settings.mcp.is_some() {
            inv.arg("--mcp-config")
                .arg(Self::mcp_config_path(settings).to_string_lossy())
                // Headless runs cannot answer a permission prompt, so without
                // this every nubi tool call is denied unless the agent also
                // runs with --skip-permissions. The `=` form keeps the
                // variadic flag from swallowing the next argument.
                .arg("--allowedTools=mcp__nubi");
        }
        if let Some(pct) = settings.compact_at_pct {
            // Claude Code's own auto-compaction, as a share of the context
            // window; the default waits until the window is nearly full.
            inv.env("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE", pct.to_string());
        }
        if let Some(ref url) = settings.llm_base_url {
            inv.env("ANTHROPIC_BASE_URL", url);
        }
        if let Some(ref key) = settings.llm_api_key {
            inv.env("ANTHROPIC_API_KEY", key);
        }
        inv
    }

    /// One `result` object. Claude prints it even when it failed, with
    /// `is_error` set and the reason in `result`.
    fn parse_output(&self, stdout: &str) -> anyhow::Result<Parsed> {
        let trimmed = stdout.trim();
        if trimmed.is_empty() {
            return Ok(Parsed::default());
        }
        // Tolerate a stray line before the JSON (a warning, say): take the
        // last line that parses as an object.
        let value = trimmed
            .lines()
            .rev()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
            .find(|v| v.is_object())
            .or_else(|| serde_json::from_str(trimmed).ok())
            .ok_or_else(|| {
                anyhow::anyhow!("claude printed something other than JSON: {}", trimmed)
            })?;

        let is_error = value["is_error"].as_bool().unwrap_or(false);
        let result = value["result"].as_str().unwrap_or_default().to_string();
        Ok(Parsed {
            text: if is_error {
                String::new()
            } else {
                result.clone()
            },
            structured: value
                .get("structured_output")
                .cloned()
                .filter(|v| !v.is_null()),
            is_error,
            error: is_error.then(|| {
                let subtype = value["subtype"].as_str().unwrap_or("error");
                match value["api_error_status"].as_u64() {
                    Some(code) => format!("{subtype} (HTTP {code}): {result}"),
                    None => format!("{subtype}: {result}"),
                }
            }),
            num_turns: value["num_turns"].as_u64(),
            session_id: value["session_id"].as_str().map(String::from),
            cost_usd: value["total_cost_usd"].as_f64(),
            duration_ms: value["duration_ms"].as_u64(),
            usage: TokenUsage {
                input: value["usage"]["input_tokens"].as_u64(),
                output: value["usage"]["output_tokens"].as_u64(),
                cache_read: value["usage"]["cache_read_input_tokens"].as_u64(),
                cache_write: value["usage"]["cache_creation_input_tokens"].as_u64(),
            },
        })
    }
}
