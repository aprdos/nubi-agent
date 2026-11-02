use std::collections::BTreeMap;

use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;
use uuid::Uuid;

use crate::runtime::TokenUsage;
use crate::types::{LogEntry, SendMessage, SendPart};

#[derive(Deserialize)]
struct CommitResp {
    committed: u64,
}

#[derive(Deserialize)]
struct PollResp {
    entries: Vec<LogEntry>,
}

#[derive(Deserialize)]
struct Me {
    space_id: String,
    agent_id: String,
}

const POST_ATTEMPTS: u32 = 3;

/// Without these a single unroutable DNS answer hangs a request forever: the
/// OS retries the SYN for ~75s, reqwest starts over, and the agent never gets
/// an error it can act on.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

const CONNECT_BACKOFF_START: Duration = Duration::from_secs(1);
const CONNECT_BACKOFF_MAX: Duration = Duration::from_secs(60);

/// Every request here is short — the SSE stream runs on its own
/// eventsource-client, so a whole-request timeout is safe.
fn http_client() -> Client {
    Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("reqwest client builds with rustls")
}

/// Metadata on every reply nubi-agent posts.
///
/// `expect_reply` defaults to `[]`, the loop-brake: unlike the engine,
/// nubi-agent falls back to parsing "@name" out of message text, so a reply
/// that merely says "@foo" in prose must not trigger foo. A reply that
/// genuinely wants an answer names who in `expect_reply`.
fn post_metadata(
    expect_reply: Option<&[String]>,
    mandate_hash: Option<&str>,
    usage: Option<serde_json::Value>,
) -> serde_json::Value {
    let mut meta = serde_json::json!({"expect_reply": expect_reply.unwrap_or(&[])});
    if let Some(hash) = mandate_hash {
        meta["mandate_hash"] = hash.into();
    }
    if let Some(usage) = usage {
        meta["usage"] = usage;
    }
    meta
}

/// `metadata.usage`: the tokens the run behind the reply used, per model,
/// and the provider that served them, for the engine to add up per agent.
/// Prices are not ours to know; nubi-app applies them. None when the CLI
/// reported no tokens.
pub fn usage_metadata(
    provider: &str,
    model_usage: &BTreeMap<String, TokenUsage>,
) -> Option<serde_json::Value> {
    let mut models: BTreeMap<String, [u64; 4]> = BTreeMap::new();
    for (model, u) in model_usage {
        let n = [u.input, u.output, u.cache_read, u.cache_write].map(Option::unwrap_or_default);
        if n.iter().all(|&t| t == 0) {
            continue;
        }
        let sum = models.entry(normalize_model(model)).or_default();
        for (s, t) in sum.iter_mut().zip(n) {
            *s += t;
        }
    }
    if models.is_empty() {
        return None;
    }
    let models: serde_json::Map<String, serde_json::Value> = models
        .into_iter()
        .map(|(model, [input, output, cache_read, cache_write])| {
            let tokens = serde_json::json!({
                "input_tokens": input,
                "output_tokens": output,
                "cache_read_tokens": cache_read,
                "cache_write_tokens": cache_write,
            });
            (model, tokens)
        })
        .collect();
    Some(serde_json::json!({"provider": provider, "models": models}))
}

/// One id per model whatever the provider calls it: lowercase, dots as
/// dashes, no provider prefix (`us.anthropic.`), no date or version suffix
/// (`-20251101`, `@20251101`, `-v1:0`).
pub fn normalize_model(model: &str) -> String {
    let mut m = model.trim().to_lowercase();
    if let Some(i) = m.find("anthropic.") {
        m = m[i + "anthropic.".len()..].to_string();
    }
    if let Some(i) = m.find('@') {
        m.truncate(i);
    }
    if let Some(i) = m.find(":") {
        m.truncate(i);
        if let Some(v) = m.rfind("-v") {
            if m[v + 2..].chars().all(|c| c.is_ascii_digit()) {
                m.truncate(v);
            }
        }
    }
    if let Some(d) = m.rfind('-') {
        let tail = &m[d + 1..];
        if tail.len() == 8 && tail.chars().all(|c| c.is_ascii_digit()) {
            m.truncate(d);
        }
    }
    m.replace('.', "-")
}

/// What to post: the text plus who it is for.
#[derive(Debug, Clone, Default)]
pub struct Outgoing<'a> {
    pub text: &'a str,
    pub mentions: &'a [String],
    pub expect_reply: Option<&'a [String]>,
    /// Hash of the mandate that was in the prompt for this reply, stamped as
    /// `metadata.mandate_hash` so an engine in strict mode can tell the reply
    /// was written under the current mandate.
    pub mandate_hash: Option<&'a str>,
    /// `metadata.usage`, from [`usage_metadata`].
    pub usage: Option<serde_json::Value>,
}

/// Nubi API client for posting messages
#[derive(Clone)]
pub struct NubiClient {
    client: Client,
    base_url: String,
    token: String,
    space: String,
    agent_id: String,
}

impl NubiClient {
    pub fn new(base_url: &str, token: &str, space: &str, agent_id: &str) -> Self {
        Self {
            client: http_client(),
            base_url: base_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
            space: space.to_string(),
            agent_id: agent_id.to_string(),
        }
    }

    /// Build a client whose space and agent_id come from the token itself.
    ///
    /// The space id sits in every request path, so one that disagrees with the
    /// key 403s every call while the key itself is perfectly fine — a failure
    /// that reads like a permissions problem and isn't. The server already
    /// knows which space a key belongs to, so ask it rather than being told.
    ///
    /// The server's own error body is passed through: "key has been revoked"
    /// is worth far more than a generic "invalid token".
    ///
    /// A transport failure here is waited out, not fatal. The SSE loop already
    /// reconnects forever, so an agent that exits because the engine happened
    /// to be unreachable at startup is one that silently stays dead long after
    /// the outage is over. A rejection still aborts — no amount of waiting
    /// fixes a revoked key.
    pub async fn connect(base_url: &str, token: &str) -> anyhow::Result<Self> {
        let base_url = base_url.trim_end_matches('/');
        let client = http_client();
        let mut backoff = CONNECT_BACKOFF_START;

        let resp = loop {
            match client
                .get(format!("{}/v1/me", base_url))
                .header("Authorization", format!("Bearer {}", token))
                .send()
                .await
            {
                Ok(resp) => break resp,
                Err(e) => {
                    eprintln!(
                        "[nubi-agent] Cannot reach nubi at {}: {}, retrying in {}s",
                        base_url,
                        e,
                        backoff.as_secs()
                    );
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(CONNECT_BACKOFF_MAX);
                }
            }
        };

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Rejected by nubi at {}: {} {}", base_url, status, body);
        }

        let me: Me = resp.json().await?;
        Ok(Self::new(base_url, token, &me.space_id, &me.agent_id))
    }

    pub fn space(&self) -> &str {
        &self.space
    }

    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Post a message to a room.
    ///
    /// Retried on transport and 5xx failures. The message_id is generated once
    /// up front so a retry is deduplicated server-side instead of double-posting.
    pub async fn post(&self, room_id: &str, out: Outgoing<'_>) -> anyhow::Result<()> {
        let msg = SendMessage {
            message_id: Uuid::new_v4().to_string(),
            parts: vec![SendPart {
                kind: "text",
                text: out.text.to_string(),
            }],
            mentions: out.mentions.to_vec(),
            metadata: Some(post_metadata(out.expect_reply, out.mandate_hash, out.usage)),
        };

        let url = format!(
            "{}/v1/spaces/{}/rooms/{}/messages",
            self.base_url, self.space, room_id
        );

        let mut last_err = String::new();
        for attempt in 1..=POST_ATTEMPTS {
            match self
                .client
                .post(&url)
                .header("Authorization", format!("Bearer {}", self.token))
                .json(&msg)
                .send()
                .await
            {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        return Ok(());
                    }
                    let body = resp.text().await.unwrap_or_default();
                    last_err = format!("{} - {}", status, body);
                    if !status.is_server_error() {
                        break;
                    }
                }
                Err(e) => last_err = e.to_string(),
            }

            if attempt < POST_ATTEMPTS {
                let backoff = Duration::from_millis(500 * (1 << (attempt - 1)));
                eprintln!(
                    "[nubi-agent] Post to {} failed ({}), retrying in {}ms",
                    room_id,
                    last_err,
                    backoff.as_millis()
                );
                tokio::time::sleep(backoff).await;
            }
        }

        anyhow::bail!("Failed to post message to {}: {}", room_id, last_err)
    }

    /// Build SSE stream URL
    pub fn stream_url(&self, rooms: &str) -> String {
        if rooms == "*" {
            format!("{}/v1/spaces/{}/stream", self.base_url, self.space)
        } else {
            format!(
                "{}/v1/spaces/{}/rooms/{}/stream",
                self.base_url, self.space, rooms
            )
        }
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    /// Commit a read offset for this room and return the server's resulting value.
    ///
    /// The server clamps monotonically, so committing 0 reads back the current
    /// offset without moving it — that is how a cold room is detected.
    pub async fn commit_offset(&self, room_id: &str, index: u64) -> anyhow::Result<u64> {
        let resp = self
            .client
            .post(format!(
                "{}/v1/spaces/{}/rooms/{}/offset",
                self.base_url, self.space, room_id
            ))
            .header("Authorization", format!("Bearer {}", self.token))
            .json(&serde_json::json!({ "index": index }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Failed to commit offset: {} - {}", status, body);
        }

        Ok(resp.json::<CommitResp>().await?.committed)
    }

    /// Fetch the newest entries in a room, oldest first.
    ///
    /// Uses `before_index` rather than a forward poll on purpose: a forward
    /// poll starts at the committed offset, so a room the agent has already
    /// read reads back empty. The server clamps `before_index` to the head,
    /// so the sentinel below means "the last `limit` entries". Neither form
    /// moves the read offset — committing is a separate call.
    pub async fn history(&self, room_id: &str, limit: usize) -> anyhow::Result<Vec<LogEntry>> {
        let resp = self
            .client
            .get(format!(
                "{}/v1/spaces/{}/rooms/{}/messages",
                self.base_url, self.space, room_id
            ))
            .header("Authorization", format!("Bearer {}", self.token))
            .query(&[
                ("before_index", u64::MAX.to_string()),
                ("limit", limit.to_string()),
            ])
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "Failed to fetch history for {}: {} - {}",
                room_id,
                status,
                body
            );
        }

        Ok(resp.json::<PollResp>().await?.entries)
    }

    /// Fetch one message by its global index, if the room has it.
    pub async fn message_at(&self, room_id: &str, index: u64) -> anyhow::Result<Option<LogEntry>> {
        let resp = self
            .client
            .get(format!(
                "{}/v1/spaces/{}/rooms/{}/messages",
                self.base_url, self.space, room_id
            ))
            .header("Authorization", format!("Bearer {}", self.token))
            .query(&[("from_index", index.to_string()), ("limit", "1".into())])
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "Failed to fetch message {} in {}: {} - {}",
                index,
                room_id,
                status,
                body
            );
        }

        Ok(resp
            .json::<PollResp>()
            .await?
            .entries
            .into_iter()
            .find(|e| e.global_index == index))
    }

    /// Set typing status in a room
    pub async fn set_typing(&self, room_id: &str, typing: bool) -> anyhow::Result<()> {
        let resp = self
            .client
            .post(format!(
                "{}/v1/spaces/{}/rooms/{}/typing",
                self.base_url, self.space, room_id
            ))
            .header("Authorization", format!("Bearer {}", self.token))
            .json(&serde_json::json!({ "typing": typing }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Failed to set typing: {} - {}", status, body);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_carries_the_mandate_hash_only_when_given() {
        assert_eq!(
            post_metadata(None, None, None),
            serde_json::json!({"expect_reply": []})
        );
        assert_eq!(
            post_metadata(None, Some("sha256:ab"), None),
            serde_json::json!({"expect_reply": [], "mandate_hash": "sha256:ab"})
        );
        let who = vec!["pine".to_string()];
        assert_eq!(
            post_metadata(Some(&who), None, None),
            serde_json::json!({"expect_reply": ["pine"]})
        );
    }

    #[test]
    fn usage_metadata_is_tokens_per_model_and_provider() {
        let mut models = BTreeMap::new();
        assert_eq!(usage_metadata("kiro", &models), None);
        let u = |input, output, cache_read| TokenUsage {
            input: Some(input),
            output: Some(output),
            cache_read,
            cache_write: None,
        };
        models.insert("claude-opus-4-5".to_string(), u(100, 20, Some(5)));
        // The same model under another name adds up under one id.
        models.insert("claude-opus-4.5".to_string(), u(1, 1, None));
        models.insert("claude-haiku-4-5-20251001".to_string(), u(7, 3, None));
        // A model that used nothing is left out.
        models.insert("claude-sonnet-4-6".to_string(), TokenUsage::default());
        assert_eq!(
            usage_metadata("kiro", &models),
            Some(serde_json::json!({
                "provider": "kiro",
                "models": {
                    "claude-haiku-4-5": {"input_tokens": 7, "output_tokens": 3,
                                         "cache_read_tokens": 0, "cache_write_tokens": 0},
                    "claude-opus-4-5": {"input_tokens": 101, "output_tokens": 21,
                                        "cache_read_tokens": 5, "cache_write_tokens": 0}
                }
            }))
        );
    }

    #[test]
    fn model_names_normalize_across_providers() {
        for (raw, want) in [
            ("claude-opus-4-5", "claude-opus-4-5"),
            ("claude-opus-4.5", "claude-opus-4-5"),
            ("Claude-Opus-4-5-20251101", "claude-opus-4-5"),
            ("claude-opus-4-5@20251101", "claude-opus-4-5"),
            (
                "us.anthropic.claude-opus-4-5-20251101-v1:0",
                "claude-opus-4-5",
            ),
            ("anthropic.claude-haiku-4-5", "claude-haiku-4-5"),
            ("claude-opus-5-5", "claude-opus-5-5"),
        ] {
            assert_eq!(normalize_model(raw), want, "{raw}");
        }
    }

    #[test]
    fn test_stream_url_all_rooms() {
        let client = NubiClient::new("http://localhost:9099", "token", "space1", "agent1");
        assert_eq!(
            client.stream_url("*"),
            "http://localhost:9099/v1/spaces/space1/stream"
        );
    }

    #[test]
    fn test_stream_url_specific_room() {
        let client = NubiClient::new("http://localhost:9099", "token", "space1", "agent1");
        assert_eq!(
            client.stream_url("general"),
            "http://localhost:9099/v1/spaces/space1/rooms/general/stream"
        );
    }

    #[test]
    fn test_base_url_trailing_slash() {
        let client = NubiClient::new("http://localhost:9099/", "token", "space1", "agent1");
        assert_eq!(
            client.stream_url("*"),
            "http://localhost:9099/v1/spaces/space1/stream"
        );
    }
}
