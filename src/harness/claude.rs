use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use super::{write_private, Harness, Invocation, OutputWatch, Parsed, Settings, REPLY_SCHEMA};
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
        // structured reply; plain text carries none of them. Streamed, so
        // [`ReplyWatch`] can stop a run that loops after it has its reply;
        // the last event is the same result object `json` prints.
        inv.arg("--output-format").arg("stream-json");
        inv.arg("--verbose");
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

    fn watch(&self) -> Option<Box<dyn OutputWatch>> {
        Some(Box::new(ReplyWatch::default()))
    }

    /// One `result` object. Claude prints it even when it failed, with
    /// `is_error` set and the reason in `result`.
    fn parse_output(&self, stdout: &str) -> anyhow::Result<Parsed> {
        let trimmed = stdout.trim();
        if trimmed.is_empty() {
            return Ok(Parsed::default());
        }
        // The result event is last in a stream; tolerate a stray line around
        // the JSON (a warning, say) by falling back to the last object.
        let objects: Vec<serde_json::Value> = trimmed
            .lines()
            .rev()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
            .filter(|v| v.is_object())
            .collect();
        let value = objects
            .iter()
            .find(|v| v["type"] == "result")
            .or_else(|| objects.first())
            .cloned()
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
            usage: usage(&value["usage"]),
            model_usage: value["modelUsage"]
                .as_object()
                .map(|models| {
                    models
                        .iter()
                        .map(|(model, u)| {
                            let usage = TokenUsage {
                                input: u["inputTokens"].as_u64(),
                                output: u["outputTokens"].as_u64(),
                                cache_read: u["cacheReadInputTokens"].as_u64(),
                                cache_write: u["cacheCreationInputTokens"].as_u64(),
                            };
                            (model.clone(), usage)
                        })
                        .collect()
                })
                .unwrap_or_default(),
        })
    }
}

/// Assistant messages a run may send after its reply was accepted before it
/// is stopped. A healthy run sends one, a short closing line.
const STEPS_AFTER_REPLY: u64 = 3;

/// Stops a Claude run that keeps going after its structured reply was
/// accepted. Claude Code ends a run only when the model answers without a
/// tool call; a model that reads the tool result of its own StructuredOutput
/// as a new, empty message (a gateway that turns a tool-result-only turn
/// into "(empty placeholder)" does that) calls StructuredOutput again and
/// again until the turn timeout.
#[derive(Default)]
struct ReplyWatch {
    session_id: Option<String>,
    /// StructuredOutput calls whose result has not come back yet, by id.
    pending: HashMap<String, serde_json::Value>,
    /// The last reply Claude accepted.
    reply: Option<serde_json::Value>,
    /// Assistant messages since `reply` was accepted.
    steps_after_reply: u64,
    /// Usage per assistant message: the stream repeats a message once per
    /// content block, with the same id and usage each time.
    messages: HashMap<String, (String, TokenUsage)>,
}

impl OutputWatch for ReplyWatch {
    fn line(&mut self, line: &str) -> Option<String> {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            return None;
        };
        // Subagents stream their own messages; only the main loop answers.
        if !event["parent_tool_use_id"].is_null() {
            return None;
        }
        match event["type"].as_str() {
            Some("system") => {
                if let Some(id) = event["session_id"].as_str() {
                    self.session_id = Some(id.to_string());
                }
            }
            Some("assistant") => {
                let message = &event["message"];
                let id = message["id"].as_str().unwrap_or_default().to_string();
                if let std::collections::hash_map::Entry::Vacant(e) = self.messages.entry(id) {
                    let model = message["model"].as_str().unwrap_or_default().to_string();
                    e.insert((model, usage(&message["usage"])));
                    if self.reply.is_some() {
                        self.steps_after_reply += 1;
                    }
                }
                for block in message["content"].as_array().into_iter().flatten() {
                    if block["type"] != "tool_use" || block["name"] != "StructuredOutput" {
                        continue;
                    }
                    if self.reply.is_some() {
                        return Some(
                            "it called StructuredOutput again after its reply was accepted".into(),
                        );
                    }
                    if let Some(id) = block["id"].as_str() {
                        self.pending.insert(id.to_string(), block["input"].clone());
                    }
                }
                if self.steps_after_reply > STEPS_AFTER_REPLY {
                    return Some(format!(
                        "it kept going for {} steps after its reply was accepted",
                        self.steps_after_reply
                    ));
                }
            }
            Some("user") => {
                for block in event["message"]["content"].as_array().into_iter().flatten() {
                    if block["type"] != "tool_result" {
                        continue;
                    }
                    let Some(input) = block["tool_use_id"]
                        .as_str()
                        .and_then(|id| self.pending.remove(id))
                    else {
                        continue;
                    };
                    // A reply that failed the schema is asked for again.
                    if block["is_error"] != true {
                        self.reply = Some(input);
                    }
                }
            }
            _ => {}
        }
        None
    }

    fn parsed(&self) -> Parsed {
        let mut total = TokenUsage::default();
        let mut model_usage: BTreeMap<String, TokenUsage> = BTreeMap::new();
        for (model, u) in self.messages.values() {
            add_usage(&mut total, u);
            add_usage(model_usage.entry(model.clone()).or_default(), u);
        }
        Parsed {
            structured: self.reply.clone(),
            num_turns: Some(self.messages.len() as u64),
            session_id: self.session_id.clone(),
            usage: total,
            model_usage,
            ..Default::default()
        }
    }
}

/// Tokens in one API message's `usage`.
fn usage(u: &serde_json::Value) -> TokenUsage {
    TokenUsage {
        input: u["input_tokens"].as_u64(),
        output: u["output_tokens"].as_u64(),
        cache_read: u["cache_read_input_tokens"].as_u64(),
        cache_write: u["cache_creation_input_tokens"].as_u64(),
    }
}

fn add_usage(total: &mut TokenUsage, u: &TokenUsage) {
    let add = |a: &mut Option<u64>, b: Option<u64>| {
        if let Some(b) = b {
            *a = Some(a.unwrap_or(0) + b);
        }
    };
    add(&mut total.input, u.input);
    add(&mut total.output, u.output);
    add(&mut total.cache_read, u.cache_read);
    add(&mut total.cache_write, u.cache_write);
}
