use eventsource_client::Client;
use futures::StreamExt;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Semaphore;
use tokio::time::interval;

use crate::buffer::RoomBuffer;
use crate::claude::Claude;
use crate::nubi::NubiClient;
use crate::types::SseData;

/// How long a room's incoming burst is allowed to settle before we pick a
/// message to answer. Collapses a backlog into one reply to the newest message.
const COALESCE_WINDOW_MS: u64 = 500;

/// How often pending read offsets are pushed to the server.
const OFFSET_FLUSH_SECS: u64 = 2;

/// Ceiling on simultaneously running claude processes across all rooms.
const MAX_CONCURRENT_REPLIES: usize = 4;

/// Configuration for the agent
#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub agent_id: String,
    pub rooms: String,
    pub system_prompt: Option<String>,
    pub skip_permissions: bool,
    pub context_size: usize,
    pub rate_limit_ms: u64,
    pub mcp_url: Option<String>,
    pub heartbeat_interval_secs: u64,
    pub llm_base_url: Option<String>,
    pub llm_api_key: Option<String>,
    pub llm_model: Option<String>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            agent_id: String::new(),
            rooms: "*".to_string(),
            system_prompt: None,
            skip_permissions: false,
            context_size: 30,
            rate_limit_ms: 5000,
            mcp_url: None,
            heartbeat_interval_secs: 30,
            llm_base_url: None,
            llm_api_key: None,
            llm_model: None,
        }
    }
}

/// The message a room worker will answer. Only the newest one per room is kept.
struct Trigger {
    sender: String,
    text: String,
}

/// Which rooms have a pending message and which already have a worker running.
#[derive(Default)]
struct Dispatch {
    pending: HashMap<String, Trigger>,
    active: HashSet<String>,
}

/// Read positions: highest index delivered per room, and what the server has acked.
#[derive(Default)]
struct Offsets {
    seen: HashMap<String, u64>,
    committed: HashMap<String, u64>,
}

/// State shared between the SSE loop, the offset flusher and the room workers.
struct Shared {
    config: AgentConfig,
    nubi: NubiClient,
    claude: Claude,
    buffer: Mutex<RoomBuffer>,
    dispatch: Mutex<Dispatch>,
    offsets: Mutex<Offsets>,
    /// Whether a room had never acked an offset when we first saw it. Decided
    /// once and never revised, so the flusher's own commits can't flip it.
    cold: Mutex<HashMap<String, bool>>,
    /// Rooms whose buffer has already been seeded with server history.
    primed: Mutex<HashSet<String>>,
    replies: Semaphore,
    start_time_ms: u64,
}

/// The main agent that listens to nubi and responds via Claude
pub struct Agent {
    shared: Arc<Shared>,
    /// Kept to ensure the temp config file isn't deleted while agent runs
    #[allow(dead_code)]
    mcp_config_path: Option<PathBuf>,
}

impl Agent {
    pub fn new(nubi: NubiClient, config: AgentConfig) -> Self {
        // Generate MCP config if mcp_url is provided
        let mcp_config_path = config
            .mcp_url
            .as_ref()
            .map(|mcp_url| Self::generate_mcp_config(&config.agent_id, nubi.token(), mcp_url));

        let workspace = Self::prepare_workspace(&config.agent_id);

        let claude = Claude::new(&config.agent_id)
            .with_skip_permissions(config.skip_permissions)
            .with_system_prompt(config.system_prompt.clone())
            .with_mcp_config(mcp_config_path.as_deref())
            .with_workspace(workspace)
            .with_llm_base_url(config.llm_base_url.clone())
            .with_llm_api_key(config.llm_api_key.clone())
            .with_model(config.llm_model.clone());

        let buffer = RoomBuffer::new()
            .with_context_size(config.context_size)
            .with_rate_limit_ms(config.rate_limit_ms);

        let start_time_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        Self {
            shared: Arc::new(Shared {
                config,
                nubi,
                claude,
                buffer: Mutex::new(buffer),
                dispatch: Mutex::new(Dispatch::default()),
                offsets: Mutex::new(Offsets::default()),
                cold: Mutex::new(HashMap::new()),
                primed: Mutex::new(HashSet::new()),
                replies: Semaphore::new(MAX_CONCURRENT_REPLIES),
                start_time_ms,
            }),
            mcp_config_path,
        }
    }

    /// Generate MCP config file for this agent
    fn generate_mcp_config(agent_id: &str, token: &str, mcp_url: &str) -> PathBuf {
        let config = serde_json::json!({
            "mcpServers": {
                "nubi": {
                    "type": "http",
                    "url": mcp_url,
                    "headers": {
                        "Authorization": format!("Bearer {}", token)
                    }
                }
            }
        });

        let path = PathBuf::from(format!("/tmp/mcp-{}.json", agent_id));
        fs::write(&path, serde_json::to_string_pretty(&config).unwrap())
            .expect("Failed to write MCP config");

        eprintln!("[nubi-agent] Generated MCP config at {:?}", path);
        path
    }

    /// A stable per-agent directory for the spawned claude to run in.
    ///
    /// Each message is a one-shot `claude -p` that remembers nothing, so the
    /// directory is the only thing that survives: CLAUDE.md in it is reloaded
    /// on every run. Agents get their own so one cannot read another's notes.
    ///
    /// Returns None if there is no home directory to anchor to, which just
    /// leaves claude in the launching cwd as before.
    fn prepare_workspace(agent_id: &str) -> Option<PathBuf> {
        let path = std::env::home_dir()?.join(".nubi-agents").join(agent_id);
        if let Err(e) = fs::create_dir_all(&path) {
            eprintln!("[nubi-agent] Cannot create workspace {:?}: {}", path, e);
            return None;
        }
        Self::seed_memory_file(&path, agent_id);
        eprintln!("[nubi-agent] Workspace at {:?}", path);
        Some(path)
    }

    /// Create CLAUDE.md on first run only.
    ///
    /// Seeded rather than left to the agent because a file that already exists
    /// and is visible in context gets appended to, while one that has to be
    /// created from nothing reliably doesn't get written at all.
    fn seed_memory_file(dir: &Path, agent_id: &str) {
        let path = dir.join("CLAUDE.md");
        if path.exists() {
            return;
        }
        let seed = format!(
            "# {} — memory\n\n\
             This file is reloaded at the start of every message you handle, and it is \
             the only thing you remember between them. Keep durable facts here: who \
             people are, what was decided, standing instructions you were given. \
             Delete anything that stops being true, and keep it short.\n\n\
             Nothing recorded yet.\n",
            agent_id
        );
        if let Err(e) = fs::write(&path, seed) {
            eprintln!("[nubi-agent] Cannot seed {:?}: {}", path, e);
        }
    }

    /// Run the agent (blocking)
    pub async fn run(&mut self) -> anyhow::Result<()> {
        // Verify setup
        Claude::check_available().await?;

        eprintln!(
            "[nubi-agent] Connected as '{}' in space '{}', listening for mentions...",
            self.shared.config.agent_id,
            self.shared.nubi.space()
        );

        let flusher = tokio::spawn(flush_offsets(Arc::clone(&self.shared)));

        let sse_url = self.shared.nubi.stream_url(&self.shared.config.rooms);
        let es = eventsource_client::ClientBuilder::for_url(&sse_url)?
            .header(
                "Authorization",
                &format!("Bearer {}", self.shared.nubi.token()),
            )?
            .build();

        let mut stream = es.stream();

        while let Some(event) = stream.next().await {
            match event {
                Ok(eventsource_client::SSE::Event(ev)) if ev.event_type == "entry" => {
                    if let Err(e) = self.shared.ingest(&ev.data).await {
                        eprintln!("[nubi-agent] Error: {}", e);
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("[nubi-agent] SSE error: {}, reconnecting...", e);
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }

        flusher.abort();
        Ok(())
    }
}

/// Push pending read offsets to the server so a reconnect resumes where we
/// stopped instead of replaying the room from the first entry.
async fn flush_offsets(shared: Arc<Shared>) {
    let mut ticker = interval(Duration::from_secs(OFFSET_FLUSH_SECS));
    loop {
        ticker.tick().await;

        let due: Vec<(String, u64)> = {
            let offsets = shared.offsets.lock().unwrap();
            offsets
                .seen
                .iter()
                .filter(|(room, idx)| offsets.committed.get(*room).copied().unwrap_or(0) < **idx)
                .map(|(room, idx)| (room.clone(), *idx))
                .collect()
        };

        for (room, idx) in due {
            match shared.nubi.commit_offset(&room, idx).await {
                Ok(committed) => {
                    shared
                        .offsets
                        .lock()
                        .unwrap()
                        .committed
                        .insert(room, committed);
                }
                Err(e) => eprintln!("[nubi-agent] Failed to commit offset for {}: {}", room, e),
            }
        }
    }
}

impl Shared {
    /// Handle one SSE entry: record its offset, buffer it for context, and
    /// queue it as the room's next trigger if it's addressed to us.
    async fn ingest(self: &Arc<Self>, data: &str) -> anyhow::Result<()> {
        let sse_data: SseData = serde_json::from_str(data)?;
        let room_id = sse_data.room_id;
        let entry = sse_data.entry;

        // Every delivered entry advances the read position, answered or not.
        self.note_offset(&room_id, entry.global_index);

        if entry.is_message() {
            self.buffer.lock().unwrap().push(&room_id, &entry);
        } else {
            return Ok(());
        }

        if entry.sender == self.config.agent_id {
            return Ok(());
        }

        // DMs always get a reply; group rooms need an explicit mention.
        let is_dm = room_id.starts_with("dm.");
        if !is_dm && !entry.mentions(&self.config.agent_id) {
            return Ok(());
        }

        // Mentioned but not asked to answer: it is already in the buffer, so
        // we read it as context on the next real trigger.
        if !entry.expects_reply(&self.config.agent_id) {
            return Ok(());
        }

        // A room we have never acked replays its entire history on connect.
        // Only there do we fall back to "ignore anything older than this process".
        if entry.timestamp_ms < self.start_time_ms && self.is_cold(&room_id).await {
            return Ok(());
        }

        self.enqueue(
            &room_id,
            Trigger {
                sender: entry.sender.clone(),
                text: entry.text(),
            },
        );

        Ok(())
    }

    fn note_offset(&self, room_id: &str, index: u64) {
        let mut offsets = self.offsets.lock().unwrap();
        let seen = offsets.seen.entry(room_id.to_string()).or_insert(0);
        if index > *seen {
            *seen = index;
        }
    }

    async fn is_cold(&self, room_id: &str) -> bool {
        if let Some(&cold) = self.cold.lock().unwrap().get(room_id) {
            return cold;
        }

        let committed = self.nubi.commit_offset(room_id, 0).await.unwrap_or(0);
        let cold = committed == 0;
        self.cold.lock().unwrap().insert(room_id.to_string(), cold);
        cold
    }

    /// Load the room's recent history into the buffer, once per process.
    ///
    /// The buffer is memory-only, so without this the first reply after every
    /// restart is written as if the room had just begun. A failure here is not
    /// fatal — answering with a thin context beats not answering at all.
    async fn prime_buffer(&self, room_id: &str) {
        if !self.primed.lock().unwrap().insert(room_id.to_string()) {
            return;
        }

        match self.nubi.history(room_id, self.config.context_size).await {
            Ok(entries) => self.buffer.lock().unwrap().prime(room_id, &entries),
            Err(e) => {
                eprintln!("[nubi-agent] Could not load history for {}: {}", room_id, e);
                self.primed.lock().unwrap().remove(room_id);
            }
        }
    }

    /// Make this the room's pending message, starting a worker if none runs.
    fn enqueue(self: &Arc<Self>, room_id: &str, trigger: Trigger) {
        let mut dispatch = self.dispatch.lock().unwrap();
        dispatch.pending.insert(room_id.to_string(), trigger);
        if !dispatch.active.insert(room_id.to_string()) {
            return;
        }
        drop(dispatch);

        let shared = Arc::clone(self);
        let room_id = room_id.to_string();
        tokio::spawn(async move { shared.serve_room(room_id).await });
    }

    /// Answer a room's pending messages one at a time until none are left.
    /// Rooms run independently, so a slow reply in one doesn't stall the others.
    async fn serve_room(self: Arc<Self>, room_id: String) {
        loop {
            tokio::time::sleep(Duration::from_millis(COALESCE_WINDOW_MS)).await;

            let wait = self.buffer.lock().unwrap().rate_limit_remaining(&room_id);
            if let Some(wait) = wait {
                tokio::time::sleep(wait).await;
            }

            let trigger = {
                let mut dispatch = self.dispatch.lock().unwrap();
                match dispatch.pending.remove(&room_id) {
                    Some(trigger) => trigger,
                    None => {
                        dispatch.active.remove(&room_id);
                        return;
                    }
                }
            };

            if let Err(e) = self.reply(&room_id, &trigger).await {
                eprintln!("[nubi-agent] Error in {}: {}", room_id, e);
            }
        }
    }

    async fn reply(&self, room_id: &str, trigger: &Trigger) -> anyhow::Result<()> {
        let _permit = self.replies.acquire().await?;

        eprintln!(
            "[nubi-agent] @{} in {}: {}",
            self.config.agent_id,
            room_id,
            trigger.text.chars().take(50).collect::<String>()
        );

        self.prime_buffer(room_id).await;

        let context = self.buffer.lock().unwrap().context(room_id);
        let prompt = self
            .claude
            .build_prompt(room_id, &trigger.sender, &trigger.text, &context);

        eprintln!("[nubi-agent] Running claude...");
        let response = self.run_with_typing(room_id, &prompt).await?;

        if response.is_empty() {
            eprintln!("[nubi-agent] Empty response, skipping");
            return Ok(());
        }

        // Skip posting short confirmations (likely after tool use)
        let trimmed = response.trim();
        let trimmed_lower = trimmed.to_lowercase();
        // Strip trailing punctuation for matching
        let trimmed_clean = trimmed_lower.trim_end_matches(['.', '!', ',']);
        let skip_words = [
            "ok",
            "done",
            "sent",
            "yes",
            "no",
            "empty",
            "understood",
            "got it",
            "will do",
            "on it",
        ];
        if trimmed_lower.len() <= 30
            && skip_words.iter().any(|w| {
                trimmed_clean == *w
                    || trimmed_lower.contains("(no response)")
                    || trimmed_lower.contains("(empty)")
            })
        {
            eprintln!(
                "[nubi-agent] Short confirmation '{}', skipping post",
                trimmed
            );
            return Ok(());
        }

        // Skip responses with prompt format leaking at start
        if trimmed.starts_with("Human:") || trimmed.starts_with("Assistant:") {
            eprintln!(
                "[nubi-agent] Prompt format leaked '{}', skipping post",
                &trimmed[..trimmed.len().min(50)]
            );
            return Ok(());
        }

        // Truncate at "Human:" if it appears mid-response (model continuing past its turn)
        let response =
            if let Some(idx) = trimmed.find("\nHuman:").or_else(|| trimmed.find(" Human:")) {
                eprintln!("[nubi-agent] Truncating at leaked 'Human:' (pos {})", idx);
                trimmed[..idx].trim().to_string()
            } else {
                trimmed.to_string()
            };

        if response.is_empty() {
            eprintln!("[nubi-agent] Response empty after truncation, skipping");
            return Ok(());
        }

        eprintln!("[nubi-agent] Responding ({} chars)", response.len());
        self.nubi.post(room_id, &response).await?;
        self.buffer.lock().unwrap().mark_responded(room_id);

        Ok(())
    }

    /// Run Claude with typing indicator instead of heartbeat messages
    async fn run_with_typing(&self, room_id: &str, prompt: &str) -> anyhow::Result<String> {
        let typing_interval_secs = 5;

        let done = Arc::new(AtomicBool::new(false));
        let done_clone = done.clone();
        let nubi = self.nubi.clone();
        let room_id_owned = room_id.to_string();

        // Start typing indicator
        if let Err(e) = self.nubi.set_typing(room_id, true).await {
            eprintln!("[nubi-agent] Failed to set typing: {}", e);
        }

        // Spawn typing refresh task
        let typing_handle = tokio::spawn(async move {
            let mut ticker = interval(Duration::from_secs(typing_interval_secs));
            ticker.tick().await; // Skip first immediate tick

            loop {
                ticker.tick().await;

                if done_clone.load(Ordering::Relaxed) {
                    break;
                }

                // Refresh typing indicator
                if let Err(e) = nubi.set_typing(&room_id_owned, true).await {
                    eprintln!("[nubi-agent] Failed to refresh typing: {}", e);
                }
            }
        });

        // Run Claude
        let result = self.claude.run(prompt).await;

        // Signal typing to stop
        done.store(true, Ordering::Relaxed);
        typing_handle.abort();

        // Clear typing indicator
        if let Err(e) = self.nubi.set_typing(room_id, false).await {
            eprintln!("[nubi-agent] Failed to clear typing: {}", e);
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_config_default() {
        let config = AgentConfig::default();
        assert_eq!(config.rooms, "*");
        assert_eq!(config.context_size, 30);
        assert_eq!(config.rate_limit_ms, 5000);
        assert!(!config.skip_permissions);
    }
}
