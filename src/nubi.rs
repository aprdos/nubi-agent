use reqwest::Client;
use uuid::Uuid;

use crate::types::{SendMessage, SendPart};

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

    /// Post a message to a room (with no_reply flag to prevent loops)
    pub async fn post(&self, room_id: &str, text: &str) -> anyhow::Result<()> {
        let msg = SendMessage {
            message_id: Uuid::new_v4().to_string(),
            parts: vec![SendPart {
                kind: "text",
                text: text.to_string(),
            }],
            mentions: vec![],
            metadata: Some(serde_json::json!({"no_reply": true})),
        };

        let resp = self
            .client
            .post(format!(
                "{}/v1/spaces/{}/rooms/{}/messages",
                self.base_url, self.space, room_id
            ))
            .header("Authorization", format!("Bearer {}", self.token))
            .json(&msg)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Failed to post message: {} - {}", status, body);
        }

        Ok(())
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
