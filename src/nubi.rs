use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;
use uuid::Uuid;

use crate::types::{SendMessage, SendPart};

#[derive(Deserialize)]
struct CommitResp {
    committed: u64,
}

const POST_ATTEMPTS: u32 = 3;

/// Nubi API client for posting messages
#[derive(Clone)]
pub struct NubiClient {
    client: Client,
    base_url: String,
    token: String,
    space: String,
}

impl NubiClient {
    pub fn new(base_url: &str, token: &str, space: &str) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
            space: space.to_string(),
        }
    }

    /// Verify the token is valid
    pub async fn verify(&self) -> anyhow::Result<()> {
        let resp = self
            .client
            .get(format!("{}/v1/me", self.base_url))
            .header("Authorization", format!("Bearer {}", self.token))
            .send()
            .await?;

        if !resp.status().is_success() {
            anyhow::bail!("Invalid token or cannot reach nubi at {}", self.base_url);
        }
        Ok(())
    }

    /// Post a message to a room.
    ///
    /// `expect_reply: []` is the loop-brake. Unlike the engine, nubi-agent
    /// falls back to parsing "@name" out of message text, so without this an
    /// agent writing "@foo" in prose would trigger foo, who answers, and so on.
    /// Agents that genuinely want a reply use the nubi_send tool instead.
    ///
    /// Retried on transport and 5xx failures. The message_id is generated once
    /// up front so a retry is deduplicated server-side instead of double-posting.
    pub async fn post(&self, room_id: &str, text: &str) -> anyhow::Result<()> {
        let msg = SendMessage {
            message_id: Uuid::new_v4().to_string(),
            parts: vec![SendPart {
                kind: "text",
                text: text.to_string(),
            }],
            mentions: vec![],
            metadata: Some(serde_json::json!({"expect_reply": []})),
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
    fn test_stream_url_all_rooms() {
        let client = NubiClient::new("http://localhost:9099", "token", "space1");
        assert_eq!(
            client.stream_url("*"),
            "http://localhost:9099/v1/spaces/space1/stream"
        );
    }

    #[test]
    fn test_stream_url_specific_room() {
        let client = NubiClient::new("http://localhost:9099", "token", "space1");
        assert_eq!(
            client.stream_url("general"),
            "http://localhost:9099/v1/spaces/space1/rooms/general/stream"
        );
    }

    #[test]
    fn test_base_url_trailing_slash() {
        let client = NubiClient::new("http://localhost:9099/", "token", "space1");
        assert_eq!(
            client.stream_url("*"),
            "http://localhost:9099/v1/spaces/space1/stream"
        );
    }
}
