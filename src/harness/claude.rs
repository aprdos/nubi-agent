use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::{write_private, Harness, Invocation, OutputWatch, Parsed, Settings, REPLY_SCHEMA};
use crate::runtime::{Session, TokenUsage};

/// Anthropic Claude Code: `claude -p`.
pub struct Claude;

impl Claude {
    fn mcp_config_path(settings: &Settings) -> PathBuf {
        settings.state_dir().join("claude-mcp.json")
    }

    /// Flags and environment both kinds of run share.
    fn common(
        inv: &mut Invocation,
        settings: &Settings,
        system_prompt: &str,
        session: Option<&Session>,
    ) {
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
            // Behind a custom base URL, Claude Code (2.1.247+) defers MCP tools
            // behind ToolSearch and `tool_reference` blocks. Through a Kiro
            // gateway the model then calls tools by mangled names (XML in the
            // name), the upstream refuses every later request carrying that
            // call, and the session is stuck. Load the tools up front instead.
            inv.env("ENABLE_TOOL_SEARCH", "false");
        }
        if let Some(ref key) = settings.llm_api_key {
            inv.env("ANTHROPIC_API_KEY", key);
        }
    }
}

impl Harness for Claude {
    fn binary(&self) -> &'static str {
        "claude"
    }

    fn env_prefixes(&self) -> Vec<&'static str> {
        // Bedrock and Vertex need the cloud's own credentials, but only when
        // Claude Code is told to use them.
        let mut p = vec!["ANTHROPIC_", "CLAUDE_"];
        if std::env::var_os("CLAUDE_CODE_USE_BEDROCK").is_some() {
            p.push("AWS_");
        }
        if std::env::var_os("CLAUDE_CODE_USE_VERTEX").is_some() {
            p.extend(["GOOGLE_", "CLOUD_ML_", "VERTEX_"]);
        }
        p
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

    fn structured_replies(&self) -> bool {
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
        Self::common(&mut inv, settings, system_prompt, session);
        inv
    }

    /// A session that stays up: messages arrive on stdin as stream-json, one
    /// `result` event ends each turn, and a background command finishing
    /// starts a turn of its own. No reply schema (Claude Code holds it to the
    /// first turn only); the agent answers with nubi_send instead.
    fn live_invocation(
        &self,
        settings: &Settings,
        system_prompt: &str,
        session: Option<&Session>,
    ) -> Option<Invocation> {
        let mut inv = Invocation::default();
        inv.arg("-p");
        inv.arg("--input-format").arg("stream-json");
        inv.arg("--output-format").arg("stream-json");
        inv.arg("--verbose");
        // Each message of ours is echoed when the session takes it up, which
        // is how a turn we started is told from one it started itself.
        inv.arg("--replay-user-messages");
        Self::common(&mut inv, settings, system_prompt, session);
        Some(inv)
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

/// Assistant messages a run may send after its reply was accepted; the next
/// one stops it. A healthy run sends one, a short closing line.
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
    /// Ids of every StructuredOutput call seen.
    calls: HashSet<String>,
    /// StructuredOutput calls whose result has not come back yet, by id.
    pending: HashMap<String, serde_json::Value>,
    /// The last reply Claude accepted. Claude Code's `structured_output` is
    /// the input of the call it accepted, so this is the same object the
    /// result event would carry.
    reply: Option<serde_json::Value>,
    /// Assistant messages since `reply` was accepted.
    steps_after_reply: u64,
    /// Usage per assistant message: the stream repeats a message once per
    /// content block, with the same id and usage each time.
    messages: HashMap<String, (String, TokenUsage)>,
    /// Events that came without an id, each counted as its own.
    unnamed: u64,
    /// Why the run failed, when the watch stopped it for a failure rather
    /// than after a reply.
    failure: Option<String>,
}

impl ReplyWatch {
    /// An event's id, or a fresh one when it has none, so events without
    /// an id are never merged into one.
    fn id_or_fresh(&mut self, id: &serde_json::Value) -> String {
        match id.as_str() {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => {
                self.unnamed += 1;
                format!("unnamed-{}", self.unnamed)
            }
        }
    }
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
                // Claude Code retries every API error up to ten times with
                // backoff, minutes in all. A rejected key will not get better.
                let status = event["error_status"].as_u64();
                if event["subtype"] == "api_retry" && matches!(status, Some(401 | 403)) {
                    let reason = format!(
                        "the LLM API rejected the key (HTTP {}). Check --llm-key / \
                         NUBI_LLM_KEY and --llm-base-url, or the CLI's own login",
                        status.unwrap_or_default()
                    );
                    self.failure = Some(reason.clone());
                    return Some(reason);
                }
            }
            Some("assistant") => {
                let message = &event["message"];
                let id = self.id_or_fresh(&message["id"]);
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
                    let call = self.id_or_fresh(&block["id"]);
                    // The same call streamed again is not a new call.
                    if !self.calls.insert(call.clone()) {
                        continue;
                    }
                    if self.reply.is_some() {
                        return Some(
                            "it called StructuredOutput again after its reply was accepted".into(),
                        );
                    }
                    self.pending.insert(call, block["input"].clone());
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
            if !model.is_empty() {
                add_usage(model_usage.entry(model.clone()).or_default(), u);
            }
        }
        Parsed {
            is_error: self.failure.is_some(),
            error: self.failure.clone(),
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
