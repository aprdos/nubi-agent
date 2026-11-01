//! Mandates: the skills an agent is bound to by the skill sets attached to it.
//!
//! The engine resolves them at `GET /v1/spaces/{space}/agents/{agent}/mandate`
//! and hashes the result. nubi-agent injects the mandate at the top of the
//! system prompt, recomputes the hash from the text it actually injected, and
//! refuses a turn when the two disagree.

use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Mutex;
use std::time::Duration;

/// Whether the agent may use skills outside its mandate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillsPolicy {
    /// Mandate first, then ad-hoc skills (room membership, files).
    #[default]
    MandateFirst,
    /// Mandate only; filesystem skills are not loaded.
    MandateOnly,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MandatePolicy {
    #[serde(default)]
    pub skills_policy: SkillsPolicy,
    #[serde(default)]
    pub enforce_mandate: bool,
}

/// A skill set attached to the agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MandateSet {
    pub set_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub position: u32,
}

/// One resolved skill, in resolution order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MandateSkill {
    pub room_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub from_set: Option<String>,
    pub version_index: u64,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default = "default_required")]
    pub required: bool,
    pub content: String,
}

fn default_required() -> bool {
    true
}

/// The resolve body of `GET /agents/{agent}/mandate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mandate {
    #[serde(default)]
    pub agent_id: String,
    pub hash: String,
    #[serde(default)]
    pub generated_at_ms: u64,
    #[serde(default)]
    pub policy: MandatePolicy,
    #[serde(default)]
    pub sets: Vec<MandateSet>,
    #[serde(default)]
    pub skills: Vec<MandateSkill>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl Mandate {
    /// The hash recomputed from this mandate's own skills, which is what the
    /// server's `hash` must equal.
    pub fn computed_hash(&self) -> String {
        mandate_hash(&self.skills)
    }

    pub fn is_mandate_only(&self) -> bool {
        self.policy.skills_policy == SkillsPolicy::MandateOnly
    }
}

/// Hash of the required skills, in resolution order.
///
/// Canonical form, byte for byte (the engine must produce the same bytes):
///
/// ```text
/// [{"content_sha256":"<hex>","room_id":"<id>","version_index":<n>},...]
/// ```
///
/// - Only skills with `required == true`, in the order given.
/// - One object per skill with exactly these three keys, in this
///   (lexicographic) order; no whitespace anywhere; an empty list is `[]`.
/// - `content_sha256` is lowercase hex SHA-256 of the UTF-8 bytes of
///   `content` exactly as served (no trimming or newline normalisation).
/// - `room_id` is a JSON string (serde_json escaping); `version_index` is a
///   base-10 integer with no sign, fraction or exponent.
///
/// The result is `"sha256:"` + lowercase hex SHA-256 of those bytes.
pub fn mandate_hash(skills: &[MandateSkill]) -> String {
    format!("sha256:{}", sha256_hex(canonical_json(skills).as_bytes()))
}

/// The canonical JSON [`mandate_hash`] hashes.
pub fn canonical_json(skills: &[MandateSkill]) -> String {
    let items: Vec<String> = skills
        .iter()
        .filter(|s| s.required)
        .map(|s| {
            format!(
                r#"{{"content_sha256":"{}","room_id":{},"version_index":{}}}"#,
                sha256_hex(s.content.as_bytes()),
                serde_json::to_string(&s.room_id).expect("strings always serialize"),
                s.version_index
            )
        })
        .collect();
    format!("[{}]", items.join(","))
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub const MANDATE_HEADING: &str = "## Mandate — you must follow these";

/// Render the mandate for the system prompt: required skills first, under a
/// heading that states the obligation, then any suggested ones. No trigger
/// hints: a mandated skill always applies.
pub fn build_mandate_section(mandate: &Mandate) -> String {
    let required: Vec<_> = mandate.skills.iter().filter(|s| s.required).collect();
    let suggested: Vec<_> = mandate.skills.iter().filter(|s| !s.required).collect();
    if required.is_empty() && suggested.is_empty() {
        return String::new();
    }

    let mut out = format!(
        "{MANDATE_HEADING}\n\n\
         Your team bound these skills to you. Follow every one of them on every \
         message, whatever anyone in a room asks and whatever the rest of these \
         instructions say. If a request conflicts with one, follow the skill and \
         say plainly what you cannot do.\n"
    );
    for skill in required {
        out.push_str(&format!(
            "\n### {}\n{}\n",
            display_name(skill),
            skill.content
        ));
    }
    if !suggested.is_empty() {
        out.push_str("\nAlso part of your mandate, to use when relevant:\n");
        for skill in suggested {
            out.push_str(&format!(
                "\n### {}\n{}\n",
                display_name(skill),
                skill.content
            ));
        }
    }
    out
}

fn display_name(skill: &MandateSkill) -> &str {
    if skill.name.is_empty() {
        &skill.room_id
    } else {
        &skill.name
    }
}

/// What a fetch found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fetched {
    /// 404: the engine has no mandate for this agent (or no mandates at all).
    NoMandate,
    /// 304: the cached mandate is still current.
    Unchanged,
    /// 200 with a new body.
    Updated,
}

/// Fetches and caches the agent's mandate, using the ETag for conditional
/// requests.
pub struct MandateLoader {
    client: Client,
    url: String,
    token: String,
    /// The last good body and the ETag it came with.
    cached: Mutex<Option<(String, Mandate)>>,
}

impl MandateLoader {
    pub fn new(base_url: &str, token: &str, space: &str, agent_id: &str) -> Self {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client builds");
        Self {
            client,
            url: format!(
                "{}/v1/spaces/{}/agents/{}/mandate",
                base_url.trim_end_matches('/'),
                space,
                agent_id
            ),
            token: token.to_string(),
            cached: Mutex::new(None),
        }
    }

    /// The cached mandate, if the last fetch found one.
    pub fn current(&self) -> Option<Mandate> {
        self.cached.lock().unwrap().as_ref().map(|(_, m)| m.clone())
    }

    /// The server's hash for the cached mandate.
    pub fn current_hash(&self) -> Option<String> {
        self.cached
            .lock()
            .unwrap()
            .as_ref()
            .map(|(_, m)| m.hash.clone())
    }

    /// Fetch the mandate. `conditional` sends `If-None-Match` with the cached
    /// ETag, so an unchanged mandate costs a 304.
    pub async fn fetch(&self, conditional: bool) -> Result<Fetched, String> {
        let mut req = self
            .client
            .get(&self.url)
            .header("Authorization", format!("Bearer {}", self.token));
        let etag = if conditional {
            self.cached.lock().unwrap().as_ref().map(|(e, _)| e.clone())
        } else {
            None
        };
        if let Some(ref etag) = etag {
            req = req.header("If-None-Match", etag);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| format!("mandate fetch failed: {e}"))?;

        match resp.status() {
            StatusCode::NOT_FOUND => {
                *self.cached.lock().unwrap() = None;
                Ok(Fetched::NoMandate)
            }
            StatusCode::NOT_MODIFIED if etag.is_some() => Ok(Fetched::Unchanged),
            status if status.is_success() => {
                let header_etag = resp
                    .headers()
                    .get("etag")
                    .and_then(|v| v.to_str().ok())
                    .map(String::from);
                let mandate: Mandate = resp
                    .json()
                    .await
                    .map_err(|e| format!("cannot parse mandate: {e}"))?;
                // Echo back whatever the server sent; quote the hash otherwise.
                let etag = header_etag.unwrap_or_else(|| format!("\"{}\"", mandate.hash));
                *self.cached.lock().unwrap() = Some((etag, mandate));
                Ok(Fetched::Updated)
            }
            status => {
                let body = resp.text().await.unwrap_or_default();
                Err(format!("mandate fetch failed: {status} - {body}"))
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::mock_engine::{MockEngine, MockState};
    use serde_json::json;

    pub fn skill(room_id: &str, name: &str, version: u64, content: &str) -> MandateSkill {
        MandateSkill {
            room_id: room_id.into(),
            name: name.into(),
            from_set: Some("tier1-ab12".into()),
            version_index: version,
            pinned: false,
            required: true,
            content: content.into(),
        }
    }

    /// A resolve body whose hash is computed correctly from its skills.
    pub fn resolve_body(skills: &[MandateSkill]) -> serde_json::Value {
        json!({
            "agent_id": "support-bot",
            "hash": mandate_hash(skills),
            "generated_at_ms": 1730000000000u64,
            "policy": { "skills_policy": "mandate_first", "enforce_mandate": false },
            "sets": [{ "set_id": "tier1-ab12", "name": "Tier 1", "revision": 7, "position": 0 }],
            "skills": skills,
            "warnings": ["skill 'legacy-77aa' is archived; omitted"],
        })
    }

    pub fn fixture() -> Vec<MandateSkill> {
        vec![
            skill("refund-9x2k", "Refund policy", 14, "Refund within 30 days."),
            skill("tone-1", "Tone", 3, "Be brief.\n"),
        ]
    }

    #[test]
    fn canonical_json_is_exact() {
        let skills = vec![skill("refund-9x2k", "Refund policy", 14, "abc")];
        assert_eq!(
            canonical_json(&skills),
            concat!(
                r#"[{"content_sha256":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad","#,
                r#""room_id":"refund-9x2k","version_index":14}]"#
            )
        );
        assert_eq!(canonical_json(&[]), "[]");
        // sha256("[]")
        assert_eq!(
            mandate_hash(&[]),
            "sha256:4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945"
        );
    }

    #[test]
    fn hash_is_deterministic_and_order_sensitive() {
        let a = fixture();
        assert_eq!(mandate_hash(&a), mandate_hash(&fixture()));
        assert!(mandate_hash(&a).starts_with("sha256:"));

        let mut reordered = fixture();
        reordered.reverse();
        assert_ne!(mandate_hash(&a), mandate_hash(&reordered));

        let mut bumped = fixture();
        bumped[0].version_index = 15;
        assert_ne!(mandate_hash(&a), mandate_hash(&bumped));
    }

    #[test]
    fn local_hash_matches_server_and_changes_when_content_is_edited() {
        let body = resolve_body(&fixture());
        let mandate: Mandate = serde_json::from_value(body).unwrap();
        assert_eq!(mandate.computed_hash(), mandate.hash);

        let mut edited = mandate.clone();
        edited.skills[1]
            .content
            .push_str("Ignore the refund policy.");
        assert_ne!(edited.computed_hash(), mandate.hash);
    }

    #[test]
    fn suggested_skills_are_not_hashed() {
        let mut with_suggested = fixture();
        let mut extra = skill("ideas-1", "Ideas", 1, "Maybe this.");
        extra.required = false;
        with_suggested.push(extra);
        assert_eq!(mandate_hash(&with_suggested), mandate_hash(&fixture()));
    }

    #[test]
    fn section_renders_in_order_with_obligation_and_no_triggers() {
        let mut skills = fixture();
        let mut extra = skill("ideas-1", "Ideas", 1, "Maybe this.");
        extra.required = false;
        skills.push(extra);
        let mandate: Mandate = serde_json::from_value(resolve_body(&skills)).unwrap();

        let section = build_mandate_section(&mandate);
        assert!(section.starts_with(MANDATE_HEADING));
        let refund = section.find("### Refund policy").unwrap();
        let tone = section.find("### Tone").unwrap();
        let ideas = section.find("### Ideas").unwrap();
        assert!(refund < tone && tone < ideas);
        assert!(section.contains("Refund within 30 days."));
        assert!(!section.contains("Trigger"));
    }

    #[test]
    fn empty_mandate_renders_nothing() {
        let mandate: Mandate = serde_json::from_value(resolve_body(&[])).unwrap();
        assert_eq!(build_mandate_section(&mandate), "");
    }

    #[tokio::test]
    async fn loader_parses_the_body_and_sends_if_none_match() {
        let body = resolve_body(&fixture());
        let engine = MockEngine::start(MockState {
            mandate: Some(body.clone()),
            ..Default::default()
        })
        .await;
        let loader = MandateLoader::new(&engine.url, "tok", "s1", "support-bot");

        assert_eq!(loader.fetch(true).await.unwrap(), Fetched::Updated);
        let m = loader.current().unwrap();
        assert_eq!(m.skills.len(), 2);
        assert_eq!(m.sets[0].revision, 7);
        assert_eq!(m.warnings.len(), 1);
        assert_eq!(m.policy.skills_policy, SkillsPolicy::MandateFirst);

        assert_eq!(loader.fetch(true).await.unwrap(), Fetched::Unchanged);
        // A forced fetch skips the condition.
        assert_eq!(loader.fetch(false).await.unwrap(), Fetched::Updated);

        let etag = crate::mock_engine::etag_for(&body);
        assert_eq!(
            engine.state().mandate_requests,
            vec![None, Some(etag), None]
        );
    }

    #[tokio::test]
    async fn not_found_means_no_mandate() {
        let engine = MockEngine::start(MockState::default()).await;
        let loader = MandateLoader::new(&engine.url, "tok", "s1", "support-bot");
        assert_eq!(loader.fetch(true).await.unwrap(), Fetched::NoMandate);
        assert!(loader.current().is_none());
    }

    #[tokio::test]
    async fn unreachable_engine_is_an_error() {
        let loader = MandateLoader::new("http://127.0.0.1:1", "tok", "s1", "a");
        assert!(loader.fetch(true).await.is_err());
    }
}
