use eventsource_client::Client;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Semaphore;
use tokio::time::interval;

use crate::buffer::RoomBuffer;
use crate::harness::{age, now_ms, CliRuntime, HarnessKind, McpServer};
use crate::mandate::{FetchError, Fetched, Mandate, MandateLoader};
use crate::nubi::{NubiClient, Outgoing};
use crate::runtime::{AgentRuntime, RunOutput, Session, ThreadAnchor, Trigger, TurnInput};
use crate::skills::{CompositeSkillLoader, RoomSkillLoader, Skill, SkillLoader};
use crate::types::{MandateUpdate, SseData};

/// How long a room's incoming burst is allowed to settle before we answer.
/// Everything that arrived by then is answered in one turn.
const COALESCE_WINDOW_MS: u64 = 500;

/// How often pending read offsets are pushed to the server.
const OFFSET_FLUSH_SECS: u64 = 2;

/// Ceiling on simultaneously running harness processes across all rooms.
const MAX_CONCURRENT_REPLIES: usize = 4;

/// Harness runs one turn may take before giving up: the first, one retry
/// after a failure, and possibly a fresh start when a resume failed.
const MAX_RUNS_PER_TURN: u32 = 3;
const RETRY_BACKOFF: Duration = Duration::from_secs(2);

/// A room's harness session is continued for this long after its last turn;
/// after that the next message starts a fresh one.
const SESSION_MAX_IDLE: Duration = Duration::from_secs(6 * 3600);

/// How many of the agent's own recent turns in a room go into the prompt.
const JOURNAL_LINES: usize = 5;

/// Startup attempts at fetching the mandate before carrying on without one
/// (unless `mandate_strict`, which keeps trying).
const MANDATE_INIT_ATTEMPTS: u32 = 3;
const MANDATE_BACKOFF_START: Duration = Duration::from_secs(1);
const MANDATE_BACKOFF_MAX: Duration = Duration::from_secs(60);

/// Check if a string is safe to use as a path segment (no traversal)
pub(crate) fn is_safe_path_segment(s: &str) -> bool {
    !s.is_empty()
        && !s.contains('/')
        && !s.contains('\\')
        && !s.contains('\0')
        && s != "."
        && s != ".."
        && !s.starts_with("..")
}

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
    /// How often the typing indicator is refreshed while a turn runs; 0
    /// sets it once and leaves it.
    pub heartbeat_interval_secs: u64,
    pub llm_base_url: Option<String>,
    pub llm_api_key: Option<String>,
    pub llm_model: Option<String>,
    pub llm_fallback_model: Option<String>,
    pub llm_effort: Option<String>,
    pub compact_at_pct: Option<u8>,
    pub turn_timeout_secs: u64,
    pub harness: HarnessKind,
    /// Do not serve any room until the mandate has been fetched, and refuse a
    /// turn while it cannot be.
    pub mandate_strict: bool,
    /// Never load filesystem skills (the client-side twin of the engine's
    /// `mandate_only` policy).
    pub no_local_skills: bool,
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
            heartbeat_interval_secs: 5,
            llm_base_url: None,
            llm_api_key: None,
            llm_model: None,
            llm_fallback_model: None,
            llm_effort: None,
            compact_at_pct: None,
            turn_timeout_secs: crate::harness::DEFAULT_TURN_TIMEOUT.as_secs(),
            harness: HarnessKind::default(),
            mandate_strict: false,
            no_local_skills: false,
        }
    }
}

/// Where the mandate stands, as far as the agent knows.
#[derive(Debug, Default, Clone)]
struct MandateStatus {
    /// A fetch has succeeded at least once (a 404 counts: it says "none").
    known: bool,
    /// The injected mandate is not known to be the engine's current one: a
    /// change was announced and not yet fetched, or a fetched mandate could
    /// not be injected. Turns are refused while this is set.
    dirty: bool,
    /// The hash the engine's last `mandate_update` announced, until a fetch
    /// returns exactly that hash. A fetch that was already in flight when the
    /// update arrived can finish with the old mandate and must not count.
    announced: Option<String>,
    /// The last fetch could not confirm the held mandate: the engine answered
    /// 404 after serving one, or 200 with a body that is not a mandate. The
    /// last good mandate stays injected; `--mandate-strict` refuses turns.
    unverified: bool,
}

/// Which rooms have pending messages and which already have a worker running.
#[derive(Default)]
struct Dispatch {
    pending: HashMap<String, Vec<Trigger>>,
    active: HashSet<String>,
}

/// Read positions: highest index delivered per room, and what the server has acked.
#[derive(Default)]
struct Offsets {
    seen: HashMap<String, u64>,
    committed: HashMap<String, u64>,
}

/// The harness conversation a room is being served in.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionRecord {
    id: String,
    last_used_ms: u64,
    /// Highest message index the session has already seen, so the next
    /// prompt only carries what came after.
    last_index: u64,
}

/// How a turn ended, for the journal and the log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Posted,
    /// The model answered through tools and asked for nothing to be posted.
    Tools,
    Failed,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Outcome::Posted => "posted",
            Outcome::Tools => "tools",
            Outcome::Failed => "failed",
        }
    }
}

/// State shared between the SSE loop, the offset flusher and the room workers.
struct Shared {
    config: AgentConfig,
    nubi: NubiClient,
    runtime: Box<dyn AgentRuntime>,
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
    /// The agent's workspace: sessions and the per-room journal live under
    /// its `.nubi/` directory. None when there is no home directory.
    workspace: Option<PathBuf>,
    sessions: Mutex<HashMap<String, SessionRecord>>,
    /// Thread anchors already looked up, None when the lookup found nothing.
    anchors: Mutex<HashMap<String, Option<ThreadAnchor>>>,
    /// Skills from room membership and files. Re-filtered against the
    /// mandate whenever it changes.
    adhoc_skills: Vec<Skill>,
    /// None when the agent was built without engine access (`Agent::new`).
    mandate: Option<MandateLoader>,
    mandate_status: Mutex<MandateStatus>,
    /// Serialises mandate refreshes so concurrent turns don't interleave.
    mandate_refresh: tokio::sync::Mutex<()>,
}

/// The main agent that listens to nubi and responds via a harness CLI
pub struct Agent {
    shared: Arc<Shared>,
}

impl Agent {
    /// Create a new agent with synchronous filesystem-only skill loading.
    /// Use `connect` for full room-based skill loading.
    pub fn new(nubi: NubiClient, config: AgentConfig) -> Self {
        let workspace = Self::prepare_workspace(&config.agent_id, config.harness);
        let skills = Self::load_skills_sync(&config.agent_id);
        let runtime = Self::build_runtime(&config, &nubi, workspace.clone());
        Self::assemble(nubi, config, Box::new(runtime), skills, workspace, None)
    }

    /// Create a new agent with async room-based skill loading.
    /// Falls back to filesystem if room API fails.
    pub async fn connect(nubi: NubiClient, config: AgentConfig) -> Self {
        let workspace = Self::prepare_workspace(&config.agent_id, config.harness);
        let skills = Self::load_skills_async(&config.agent_id, &nubi).await;
        let runtime = Self::build_runtime(&config, &nubi, workspace.clone());
        let mandate =
            MandateLoader::new(nubi.base_url(), nubi.token(), nubi.space(), nubi.agent_id());

        let agent = Self::assemble(
            nubi,
            config,
            Box::new(runtime),
            skills,
            workspace,
            Some(mandate),
        );
        agent.shared.init_mandate().await;
        agent
    }

    fn build_runtime(
        config: &AgentConfig,
        nubi: &NubiClient,
        workspace: Option<PathBuf>,
    ) -> CliRuntime {
        let mcp = config.mcp_url.as_ref().map(|url| McpServer {
            url: url.clone(),
            token: nubi.token().to_string(),
        });
        let team_memory = dirs::home_dir().map(|h| h.join(".nubi").join("TEAM.md"));
        CliRuntime::new(config.harness, &config.agent_id)
            .with_skip_permissions(config.skip_permissions)
            .with_system_prompt(config.system_prompt.clone())
            .with_mcp(mcp)
            .with_workspace(workspace)
            .with_llm_base_url(config.llm_base_url.clone())
            .with_llm_api_key(config.llm_api_key.clone())
            .with_model(config.llm_model.clone())
            .with_fallback_model(config.llm_fallback_model.clone())
            .with_effort(config.llm_effort.clone())
            .with_compact_at_pct(config.compact_at_pct)
            .with_turn_timeout(Duration::from_secs(config.turn_timeout_secs.max(1)))
            .with_team_memory(team_memory)
    }

    /// Wire the shared state together and inject the ad-hoc skills.
    fn assemble(
        nubi: NubiClient,
        config: AgentConfig,
        runtime: Box<dyn AgentRuntime>,
        skills: Vec<Skill>,
        workspace: Option<PathBuf>,
        mandate: Option<MandateLoader>,
    ) -> Self {
        let buffer = RoomBuffer::new()
            .with_context_size(config.context_size)
            .with_rate_limit_ms(config.rate_limit_ms);

        let start_time_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let sessions = workspace
            .as_ref()
            .map(|ws| load_sessions(&sessions_path(ws)))
            .unwrap_or_default();

        let shared = Shared {
            config,
            nubi,
            runtime,
            buffer: Mutex::new(buffer),
            dispatch: Mutex::new(Dispatch::default()),
            offsets: Mutex::new(Offsets::default()),
            cold: Mutex::new(HashMap::new()),
            primed: Mutex::new(HashSet::new()),
            replies: Semaphore::new(MAX_CONCURRENT_REPLIES),
            start_time_ms,
            workspace,
            sessions: Mutex::new(sessions),
            anchors: Mutex::new(HashMap::new()),
            adhoc_skills: skills.clone(),
            mandate,
            mandate_status: Mutex::new(MandateStatus::default()),
            mandate_refresh: tokio::sync::Mutex::new(()),
        };
        if let Err(e) = shared.apply_mandate() {
            eprintln!("[nubi-agent] Cannot inject skills: {}", e);
        }
        if !skills.is_empty() {
            eprintln!("[nubi-agent] Loaded {} skill(s)", skills.len());
        }

        Self {
            shared: Arc::new(shared),
        }
    }

    /// The shared and per-agent skill directories, created if missing.
    fn skill_dirs(agent_id: &str) -> Option<(PathBuf, PathBuf)> {
        let home = dirs::home_dir()?;
        // Validate agent_id to prevent path traversal (CWE-22)
        if !is_safe_path_segment(agent_id) {
            eprintln!(
                "[nubi-agent] Invalid agent_id for skills path: {}",
                agent_id
            );
            return None;
        }
        let shared_dir = home.join(".nubi").join("skills");
        let agent_skills_dir = home.join(".nubi-agents").join(agent_id).join("skills");
        let _ = fs::create_dir_all(&shared_dir);
        let _ = fs::create_dir_all(&agent_skills_dir);
        Some((shared_dir, agent_skills_dir))
    }

    /// Load skills from filesystem only (synchronous).
    fn load_skills_sync(agent_id: &str) -> Vec<Skill> {
        let Some((shared_dir, agent_dir)) = Self::skill_dirs(agent_id) else {
            return Vec::new();
        };
        SkillLoader::new(shared_dir, Some(agent_dir)).load_all()
    }

    /// Load skills from skill rooms and the filesystem. Filesystem skills are
    /// always read; whether they are injected is decided against the mandate
    /// in `adhoc_skills`.
    async fn load_skills_async(agent_id: &str, nubi: &NubiClient) -> Vec<Skill> {
        let Some((shared_dir, agent_dir)) = Self::skill_dirs(agent_id) else {
            return Vec::new();
        };
        let fs_loader = SkillLoader::new(shared_dir, Some(agent_dir));
        let room_loader = RoomSkillLoader::new(nubi.base_url(), nubi.token(), nubi.space());
        CompositeSkillLoader::new(Some(room_loader), fs_loader)
            .load_all()
            .await
    }

    /// A stable per-agent directory for the spawned CLI to run in.
    ///
    /// The directory survives between runs: the harness's memory file in it
    /// (CLAUDE.md, AGENTS.md, GEMINI.md) is reloaded on every run, and the
    /// `.nubi/` directory holds sessions and the room journal. Agents get
    /// their own so one cannot read another's notes.
    ///
    /// Returns None if there is no home directory to anchor to, which just
    /// leaves the CLI in the launching cwd as before.
    fn prepare_workspace(agent_id: &str, harness: HarnessKind) -> Option<PathBuf> {
        // Validate agent_id to prevent path traversal (CWE-22)
        if !is_safe_path_segment(agent_id) {
            eprintln!("[nubi-agent] Invalid agent_id for workspace: {}", agent_id);
            return None;
        }

        let path = dirs::home_dir()?.join(".nubi-agents").join(agent_id);
        if let Err(e) = fs::create_dir_all(path.join(".nubi").join("rooms")) {
            eprintln!("[nubi-agent] Cannot create workspace {:?}: {}", path, e);
            return None;
        }
        Self::seed_memory_file(&path, harness.harness().memory_file(), agent_id);
        eprintln!("[nubi-agent] Workspace at {:?}", path);
        Some(path)
    }

    /// Create the memory file on first run only.
    ///
    /// Seeded rather than left to the agent because a file that already exists
    /// and is visible in context gets appended to, while one that has to be
    /// created from nothing reliably doesn't get written at all.
    fn seed_memory_file(dir: &Path, memory_file: &str, agent_id: &str) {
        let path = dir.join(memory_file);
        if path.exists() {
            return;
        }
        let seed = format!(
            "# {} — memory\n\n\
             This file is loaded at the start of every message you handle. Keep durable \
             facts here: who people are, what was decided, standing instructions you were \
             given. Delete anything that stops being true, and keep it short.\n\n\
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
        self.shared.runtime.check_available().await?;

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
                Ok(eventsource_client::SSE::Event(ev)) if ev.event_type == "mandate_update" => {
                    // Off the SSE loop: a slow mandate endpoint must not hold
                    // up room ingestion. Refreshes serialise on their own lock.
                    let shared = Arc::clone(&self.shared);
                    tokio::spawn(async move { shared.on_mandate_update(&ev.data).await });
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

/// A hash for a log line, or "none".
fn describe(hash: Option<&str>) -> &str {
    hash.unwrap_or("none")
}

/// The ad-hoc skills: skills from room membership and files, minus any that
/// collide with a mandated skill by room id or name (so a local file cannot
/// shadow a mandate), and minus files when local skills are off.
fn adhoc_skills(skills: &[Skill], mandate: Option<&Mandate>, allow_local: bool) -> Vec<Skill> {
    let mandated = |skill: &Skill| {
        mandate.is_some_and(|m| {
            m.skills.iter().any(|ms| {
                // Room ids are engine identifiers; names are typed by people
                skill.room_id() == Some(ms.room_id.as_str())
                    || skill.meta.name.trim().eq_ignore_ascii_case(ms.name.trim())
            })
        })
    };
    skills
        .iter()
        .filter(|s| allow_local || s.room_id().is_some())
        .filter(|s| !mandated(s))
        .cloned()
        .collect()
}

fn sessions_path(workspace: &Path) -> PathBuf {
    workspace.join(".nubi").join("sessions.json")
}

fn load_sessions(path: &Path) -> HashMap<String, SessionRecord> {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// `thread.<parent>.<index>` → (parent, index).
fn parse_thread_room(room_id: &str) -> Option<(String, u64)> {
    let rest = room_id.strip_prefix("thread.")?;
    let (parent, index) = rest.rsplit_once('.')?;
    Some((parent.to_string(), index.parse().ok()?))
}

/// One line, at most `max` characters.
fn short(text: &str, max: usize) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        one_line
    } else {
        format!("{}…", one_line.chars().take(max).collect::<String>())
    }
}

impl Shared {
    /// Inject the cached mandate and the ad-hoc skills that survive it.
    fn apply_mandate(&self) -> anyhow::Result<()> {
        let mandate = self.mandate.as_ref().and_then(MandateLoader::current);
        let allow_local =
            !self.config.no_local_skills && !mandate.as_ref().is_some_and(Mandate::is_mandate_only);
        let adhoc = adhoc_skills(&self.adhoc_skills, mandate.as_ref(), allow_local);
        self.runtime.inject_mandate(mandate.as_ref(), &adhoc)
    }

    /// Fetch the mandate and re-inject it when needed. `conditional` sends
    /// the cached ETag; an unconditional fetch always re-injects.
    ///
    /// The status only moves to "current" once the new mandate is injected
    /// (prompt file rewritten included) and its hash is the one the engine
    /// last announced. A failed injection leaves the previous mandate in the
    /// prompt and the status dirty, so the turn is refused and the next check
    /// retries. A 404 after a mandate was held, or a body that does not
    /// parse, keeps the last good mandate and marks it unverified.
    async fn refresh_mandate(&self, conditional: bool) -> Result<(), String> {
        let Some(loader) = &self.mandate else {
            return Ok(());
        };
        let _guard = self.mandate_refresh.lock().await;

        let fetched = match loader.fetch(conditional).await {
            Ok(f) => f,
            Err(FetchError::Parse(e)) => {
                self.mandate_status.lock().unwrap().unverified = true;
                eprintln!(
                    "[nubi-agent] ERROR: the engine's mandate body is not a mandate ({e}); \
                     keeping the last good one {}",
                    describe(loader.current_hash().as_deref())
                );
                return Err(format!("cannot parse mandate: {e}"));
            }
            Err(e @ FetchError::Transport(_)) => return Err(e.to_string()),
        };
        if fetched == Fetched::Gone {
            self.mandate_status.lock().unwrap().unverified = true;
            eprintln!(
                "[nubi-agent] ERROR: the engine no longer serves the mandate (404); \
                 keeping the last good one {}",
                describe(loader.current_hash().as_deref())
            );
            return Err("the engine no longer serves the mandate (404)".into());
        }

        let needs_apply = match fetched {
            Fetched::Updated => true,
            Fetched::NoMandate => self.runtime.mandate_hash().is_some(),
            // A dirty status after a 304 means the last injection failed (or
            // an announced change is still pending): inject again.
            Fetched::Unchanged | Fetched::Gone => self.mandate_status.lock().unwrap().dirty,
        } || !conditional;

        if needs_apply {
            match loader.current() {
                Some(m) => {
                    let revisions: Vec<String> = m
                        .sets
                        .iter()
                        .map(|s| format!("{}@{}", s.set_id, s.revision))
                        .collect();
                    eprintln!(
                        "[nubi-agent] Mandate {}: {} skill(s) from [{}]",
                        m.hash,
                        m.skills.len(),
                        revisions.join(", ")
                    );
                    for w in &m.warnings {
                        eprintln!("[nubi-agent] Mandate warning: {}", w);
                    }
                }
                None => eprintln!("[nubi-agent] No mandate"),
            }
            if let Err(e) = self.apply_mandate() {
                self.mandate_status.lock().unwrap().dirty = true;
                return Err(format!("cannot inject mandate: {e}"));
            }
        }

        let mut status = self.mandate_status.lock().unwrap();
        status.known = true;
        status.unverified = false;
        let server = loader.current_hash();
        let outstanding = status
            .announced
            .as_deref()
            .filter(|a| server.as_deref() != Some(*a))
            .map(String::from);
        match outstanding {
            Some(announced) => {
                status.dirty = true;
                eprintln!(
                    "[nubi-agent] Engine announced mandate {} but served {}; holding replies",
                    announced,
                    describe(server.as_deref())
                );
            }
            None => {
                status.dirty = false;
                status.announced = None;
            }
        }
        Ok(())
    }

    /// First mandate fetch, before any room is served. A transient failure is
    /// retried with backoff; `mandate_strict` retries until it succeeds.
    async fn init_mandate(&self) {
        if self.mandate.is_none() {
            return;
        }
        let mut backoff = MANDATE_BACKOFF_START;
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.refresh_mandate(false).await {
                Ok(()) => return,
                Err(e) if self.config.mandate_strict || attempt < MANDATE_INIT_ATTEMPTS => {
                    eprintln!("[nubi-agent] {}, retrying in {}s", e, backoff.as_secs());
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MANDATE_BACKOFF_MAX);
                }
                Err(e) => {
                    eprintln!(
                        "[nubi-agent] {}; starting without a mandate, will retry before each reply",
                        e
                    );
                    return;
                }
            }
        }
    }

    /// SSE `mandate_update`: refetch now so the next turn (and any on-disk
    /// prompt file) has the new mandate. A failed refetch leaves the mandate
    /// marked stale, and turns are refused until a refetch succeeds.
    async fn on_mandate_update(&self, data: &str) {
        let update: MandateUpdate = match serde_json::from_str(data) {
            Ok(u) => u,
            Err(e) => {
                eprintln!("[nubi-agent] Bad mandate_update event: {}", e);
                return;
            }
        };
        if !self.announce_mandate(&update) {
            return;
        }
        if let Err(e) = self.refresh_mandate(true).await {
            eprintln!(
                "[nubi-agent] {}; replies are held until it can be fetched",
                e
            );
        }
    }

    /// Record an announced mandate hash. Returns whether a refetch is due:
    /// false for another agent's update or a hash we already hold.
    fn announce_mandate(&self, update: &MandateUpdate) -> bool {
        let Some(loader) = &self.mandate else {
            return false;
        };
        if update.agent_id != self.config.agent_id {
            return false;
        }
        let current = loader.current_hash();
        if current.as_deref() == Some(update.hash.as_str())
            && self.runtime.mandate_hash() == current
            && !self.mandate_status.lock().unwrap().dirty
        {
            return false;
        }

        {
            let mut status = self.mandate_status.lock().unwrap();
            status.dirty = true;
            status.announced = Some(update.hash.clone());
        }
        eprintln!(
            "[nubi-agent] Mandate changed (revision {}), refetching",
            update
                .revision
                .map_or_else(|| "?".to_string(), |r| r.to_string())
        );
        true
    }

    /// Whether the injected mandate is the engine's current one: its hash is
    /// the server's, and no announced change is still outstanding.
    fn mandate_in_sync(&self) -> bool {
        let Some(loader) = &self.mandate else {
            return true;
        };
        let status = self.mandate_status.lock().unwrap();
        status.announced.is_none()
            && !status.dirty
            && self.runtime.mandate_hash() == loader.current_hash()
    }

    /// Before a turn: make sure the mandate in the prompt is the engine's
    /// current one, or say why the turn must be refused. The hash stamped on
    /// the reply is taken by the runtime from the prompt it actually runs.
    ///
    /// The injected hash is recomputed from the skills that went into the
    /// prompt, so a dropped or edited skill shows up as a mismatch. A
    /// mismatch, or an announced hash the conditional fetch did not deliver,
    /// gets one unconditional refetch; if it persists the turn is refused.
    async fn check_mandate(&self) -> Result<(), String> {
        if self.mandate.is_none() {
            return Ok(());
        }

        if let Err(e) = self.refresh_mandate(true).await {
            let status = self.mandate_status.lock().unwrap().clone();
            if status.dirty {
                return Err(format!("the mandate changed and cannot be refetched: {e}"));
            }
            if self.config.mandate_strict && !status.known {
                return Err(format!(
                    "no mandate fetched yet and --mandate-strict is on: {e}"
                ));
            }
            if self.config.mandate_strict && status.unverified {
                return Err(format!(
                    "the mandate cannot be verified and --mandate-strict is on: {e}"
                ));
            }
            eprintln!(
                "[nubi-agent] Mandate check skipped, using the last one fetched: {}",
                e
            );
        }

        if self.mandate_in_sync() {
            return Ok(());
        }

        eprintln!(
            "[nubi-agent] Injected mandate {} is not the engine's current one, refetching",
            describe(self.runtime.mandate_hash().as_deref())
        );
        self.refresh_mandate(false)
            .await
            .map_err(|e| format!("mandate refetch failed: {e}"))?;
        if self.mandate_in_sync() {
            return Ok(());
        }
        let status = self.mandate_status.lock().unwrap().clone();
        Err(format!(
            "injected mandate hashes to {} but the engine says {}{}",
            describe(self.runtime.mandate_hash().as_deref()),
            describe(
                self.mandate
                    .as_ref()
                    .and_then(MandateLoader::current_hash)
                    .as_deref()
            ),
            status
                .announced
                .map(|a| format!(" and announced {a}"))
                .unwrap_or_default()
        ))
    }

    /// Handle one SSE entry: record its offset, buffer it for context, and
    /// queue it as a pending trigger for its room if it's addressed to us.
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
                index: entry.global_index,
                expect_reply: entry.expect_reply_list(),
                timestamp_ms: entry.timestamp_ms,
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

    /// The message a thread room was opened on, looked up once per room.
    async fn thread_anchor(&self, room_id: &str) -> Option<ThreadAnchor> {
        if let Some(cached) = self.anchors.lock().unwrap().get(room_id) {
            return cached.clone();
        }
        let (parent, index) = parse_thread_room(room_id)?;
        let anchor = match self.nubi.message_at(&parent, index).await {
            Ok(Some(entry)) => Some(ThreadAnchor {
                parent_room: parent,
                index,
                sender: entry.sender.clone(),
                text: entry.text(),
            }),
            Ok(None) => None,
            Err(e) => {
                eprintln!(
                    "[nubi-agent] Could not load thread anchor for {}: {}",
                    room_id, e
                );
                // Not cached: the next turn tries again.
                return None;
            }
        };
        self.anchors
            .lock()
            .unwrap()
            .insert(room_id.to_string(), anchor.clone());
        anchor
    }

    fn journal_path(&self, room_id: &str) -> Option<PathBuf> {
        if !is_safe_path_segment(room_id) {
            return None;
        }
        Some(
            self.workspace
                .as_ref()?
                .join(".nubi")
                .join("rooms")
                .join(format!("{room_id}.log")),
        )
    }

    /// The agent's last few turns in a room, newest last, as prompt lines.
    fn journal_tail(&self, room_id: &str) -> Vec<String> {
        let Some(path) = self.journal_path(room_id) else {
            return Vec::new();
        };
        let Ok(text) = fs::read_to_string(path) else {
            return Vec::new();
        };
        let now = now_ms();
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        lines
            .iter()
            .rev()
            .take(JOURNAL_LINES)
            .rev()
            .map(|line| match line.split_once('\t') {
                Some((ms, rest)) => match ms.parse::<u64>() {
                    Ok(ms) => format!("{} {}", age(ms, now), rest),
                    Err(_) => rest.to_string(),
                },
                None => line.to_string(),
            })
            .collect()
    }

    fn journal_append(&self, room_id: &str, line: &str) {
        let Some(path) = self.journal_path(room_id) else {
            return;
        };
        use std::io::Write;
        let entry = format!("{}\t{}\n", now_ms(), short(line, 300));
        let written = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| f.write_all(entry.as_bytes()));
        if let Err(e) = written {
            eprintln!("[nubi-agent] Cannot write journal {:?}: {}", path, e);
        }
    }

    /// The session to run this room's turn in, when the runtime keeps them:
    /// the recorded one if it was used recently, else a fresh id.
    fn session_for(&self, room_id: &str) -> Option<Session> {
        if !self.runtime.supports_sessions() {
            return None;
        }
        let now = now_ms();
        let existing = self.sessions.lock().unwrap().get(room_id).cloned();
        match existing {
            Some(rec)
                if now.saturating_sub(rec.last_used_ms) < SESSION_MAX_IDLE.as_millis() as u64 =>
            {
                Some(Session {
                    id: rec.id,
                    resume: true,
                })
            }
            _ => Some(Session {
                id: uuid::Uuid::new_v4().to_string(),
                resume: false,
            }),
        }
    }

    fn session_last_index(&self, room_id: &str) -> u64 {
        self.sessions
            .lock()
            .unwrap()
            .get(room_id)
            .map(|r| r.last_index)
            .unwrap_or(0)
    }

    fn forget_session(&self, room_id: &str) {
        self.sessions.lock().unwrap().remove(room_id);
        self.save_sessions();
    }

    fn record_session(&self, room_id: &str, id: String, last_index: u64) {
        self.sessions.lock().unwrap().insert(
            room_id.to_string(),
            SessionRecord {
                id,
                last_used_ms: now_ms(),
                last_index,
            },
        );
        self.save_sessions();
    }

    fn save_sessions(&self) {
        let Some(ref ws) = self.workspace else {
            return;
        };
        let path = sessions_path(ws);
        let Some(parent) = path.parent() else {
            eprintln!("[nubi-agent] Sessions path {:?} has no directory", path);
            return;
        };
        let json = serde_json::to_string_pretty(&*self.sessions.lock().unwrap())
            .expect("session map serializes");
        if let Err(e) = fs::create_dir_all(parent).and_then(|_| fs::write(&path, json)) {
            eprintln!("[nubi-agent] Cannot save sessions to {:?}: {}", path, e);
        }
    }

    /// Make this one of the room's pending messages, starting a worker if none runs.
    fn enqueue(self: &Arc<Self>, room_id: &str, trigger: Trigger) {
        let mut dispatch = self.dispatch.lock().unwrap();
        dispatch
            .pending
            .entry(room_id.to_string())
            .or_default()
            .push(trigger);
        if !dispatch.active.insert(room_id.to_string()) {
            return;
        }
        drop(dispatch);

        let shared = Arc::clone(self);
        let room_id = room_id.to_string();
        tokio::spawn(async move { shared.serve_room(room_id).await });
    }

    /// Answer a room's pending messages, all at once, until none are left.
    /// Rooms run independently, so a slow reply in one doesn't stall the others.
    async fn serve_room(self: Arc<Self>, room_id: String) {
        loop {
            tokio::time::sleep(Duration::from_millis(COALESCE_WINDOW_MS)).await;

            let wait = self.buffer.lock().unwrap().rate_limit_remaining(&room_id);
            if let Some(wait) = wait {
                tokio::time::sleep(wait).await;
            }

            let triggers = {
                let mut dispatch = self.dispatch.lock().unwrap();
                match dispatch.pending.remove(&room_id) {
                    Some(triggers) if !triggers.is_empty() => triggers,
                    _ => {
                        dispatch.active.remove(&room_id);
                        return;
                    }
                }
            };

            if let Err(e) = self.reply(&room_id, &triggers).await {
                eprintln!("[nubi-agent] Error in {}: {}", room_id, e);
            }
        }
    }

    /// One turn: run the harness for every pending message in the room, post
    /// what it says, and tell the room when that could not be done.
    async fn reply(&self, room_id: &str, triggers: &[Trigger]) -> anyhow::Result<()> {
        let Some(newest) = triggers.last() else {
            return Ok(());
        };
        let _permit = self.replies.acquire().await?;

        if let Err(reason) = self.check_mandate().await {
            eprintln!("[nubi-agent] Refusing to answer in {}: {}", room_id, reason);
            return Ok(());
        }

        eprintln!(
            "[nubi-agent] @{} in {}: {}{}",
            self.config.agent_id,
            room_id,
            short(&newest.text, 50),
            if triggers.len() > 1 {
                format!(" (+{} more)", triggers.len() - 1)
            } else {
                String::new()
            }
        );

        self.prime_buffer(room_id).await;
        let thread = self.thread_anchor(room_id).await;
        let journal = self.journal_tail(room_id);
        let indexes: Vec<u64> = triggers.iter().map(|t| t.index).collect();

        let started = Instant::now();
        let mut session = self.session_for(room_id);
        let mut runs = 0u32;
        let mut nudged = false;
        let outcome: Result<RunOutput, String> = loop {
            runs += 1;
            let resumed = session.as_ref().is_some_and(|s| s.resume);
            let mut context = self
                .buffer
                .lock()
                .unwrap()
                .context_excluding(room_id, &indexes);
            if resumed {
                let last = self.session_last_index(room_id);
                context.retain(|m| m.global_index > last);
            }
            let turn = TurnInput {
                room_id: room_id.to_string(),
                triggers: triggers.to_vec(),
                context,
                thread: thread.clone(),
                journal: journal.clone(),
                resumed,
            };
            let mut prompt = self.runtime.build_prompt(&turn);
            if nudged {
                prompt.push_str(
                    "\n\n[Your previous attempt produced no reply at all. Answer in text, or \
                     set post to false if you already answered this room with nubi_send.]",
                );
            }

            eprintln!(
                "[nubi-agent] Running {:?}{}...",
                self.config.harness,
                match &session {
                    Some(s) if s.resume => " (resuming session)",
                    Some(_) => " (new session)",
                    None => "",
                }
            );
            let result = self
                .run_with_typing(room_id, &prompt, session.as_ref())
                .await;

            let last_error = match result {
                Ok(out) if !out.is_error && !out.is_silent() => break Ok(out),
                Ok(out) if !out.is_error && !nudged => {
                    eprintln!("[nubi-agent] No reply from the model, asking once more");
                    nudged = true;
                    if runs < MAX_RUNS_PER_TURN {
                        continue;
                    }
                    break Err("the model produced no reply".into());
                }
                Ok(out) => out
                    .error
                    .unwrap_or_else(|| "the model produced no reply".into()),
                Err(e) => e.to_string(),
            };

            eprintln!(
                "[nubi-agent] Run failed in {}: {}",
                room_id,
                short(&last_error, 200)
            );
            if runs >= MAX_RUNS_PER_TURN {
                break Err(last_error);
            }
            if resumed {
                // The saved conversation may be gone or corrupt; a fresh one
                // costs only the carried context.
                eprintln!(
                    "[nubi-agent] Dropping the session for {} and starting fresh",
                    room_id
                );
                self.forget_session(room_id);
                session = self.session_for(room_id);
            } else {
                tokio::time::sleep(RETRY_BACKOFF).await;
            }
        };

        let elapsed_ms = started.elapsed().as_millis() as u64;
        let session_kind = match &session {
            Some(s) if s.resume => "resumed",
            Some(_) => "new",
            None => "none",
        };

        let out = match outcome {
            Ok(out) => out,
            Err(cause) => {
                let apology = format!(
                    "Couldn't finish that one ({}). Try again or ping me.",
                    short(&cause, 160)
                );
                if let Err(e) = self
                    .nubi
                    .post(
                        room_id,
                        Outgoing {
                            text: &apology,
                            ..Default::default()
                        },
                    )
                    .await
                {
                    eprintln!(
                        "[nubi-agent] Could not report the failure in {}: {}",
                        room_id, e
                    );
                }
                self.buffer.lock().unwrap().mark_responded(room_id);
                self.journal_append(
                    room_id,
                    &format!(
                        "#{} from {}: failed: {}",
                        newest.index, newest.sender, cause
                    ),
                );
                self.log_turn(
                    room_id,
                    newest.index,
                    Outcome::Failed,
                    runs,
                    elapsed_ms,
                    None,
                    session_kind,
                );
                return Ok(());
            }
        };

        // Remember the conversation for the room's next message.
        if let Some(s) = &session {
            let id = out.session_id.clone().unwrap_or_else(|| s.id.clone());
            self.record_session(room_id, id, newest.index);
        }

        let (post, text, mentions, expect_reply) = match out.reply.clone() {
            Some(r) => (r.post, r.text, r.mentions, r.expect_reply),
            None => (true, out.text.clone(), Vec::new(), None),
        };
        let text = text.trim().to_string();
        let outcome = if !post || text.is_empty() {
            Outcome::Tools
        } else {
            Outcome::Posted
        };

        let summary = match outcome {
            Outcome::Tools => {
                eprintln!("[nubi-agent] Answered via tools, nothing to post");
                "answered via tools".to_string()
            }
            _ => {
                eprintln!(
                    "[nubi-agent] Responding ({} chars{})",
                    text.len(),
                    if expect_reply.as_ref().is_some_and(|w| !w.is_empty()) {
                        format!(", reply from {}", expect_reply.as_ref().unwrap().join(", "))
                    } else {
                        String::new()
                    }
                );
                self.nubi
                    .post(
                        room_id,
                        Outgoing {
                            text: &text,
                            mentions: &mentions,
                            expect_reply: expect_reply.as_deref(),
                            mandate_hash: out.mandate_hash.as_deref(),
                        },
                    )
                    .await?;
                format!("replied: {}", short(&text, 160))
            }
        };
        self.buffer.lock().unwrap().mark_responded(room_id);
        self.journal_append(
            room_id,
            &format!(
                "#{} from {} ({}): {}",
                newest.index,
                newest.sender,
                short(&newest.text, 60),
                summary
            ),
        );
        self.log_turn(
            room_id,
            newest.index,
            outcome,
            runs,
            elapsed_ms,
            Some(&out),
            session_kind,
        );

        Ok(())
    }

    /// One structured line per turn, for grepping and for evals.
    #[allow(clippy::too_many_arguments)]
    fn log_turn(
        &self,
        room_id: &str,
        index: u64,
        outcome: Outcome,
        runs: u32,
        elapsed_ms: u64,
        out: Option<&RunOutput>,
        session: &str,
    ) {
        let mut line = format!(
            "[nubi-agent] turn room={} idx={} outcome={} runs={} elapsed_ms={} session={}",
            room_id,
            index,
            outcome.as_str(),
            runs,
            elapsed_ms,
            session
        );
        if let Some(out) = out {
            if let Some(n) = out.num_turns {
                line.push_str(&format!(" model_turns={n}"));
            }
            if let Some(c) = out.cost_usd {
                line.push_str(&format!(" cost_usd={c:.4}"));
            }
            if let Some(d) = out.duration_ms {
                line.push_str(&format!(" cli_ms={d}"));
            }
        }
        eprintln!("{line}");
    }

    /// Run the harness with a typing indicator held for as long as it runs.
    async fn run_with_typing(
        &self,
        room_id: &str,
        prompt: &str,
        session: Option<&Session>,
    ) -> anyhow::Result<RunOutput> {
        let done = Arc::new(AtomicBool::new(false));
        let done_clone = done.clone();
        let nubi = self.nubi.clone();
        let room_id_owned = room_id.to_string();

        // Start typing indicator
        if let Err(e) = self.nubi.set_typing(room_id, true).await {
            eprintln!("[nubi-agent] Failed to set typing: {}", e);
        }

        // Refresh it while the run lasts, if asked to.
        let refresh_secs = self.config.heartbeat_interval_secs;
        let typing_handle = (refresh_secs > 0).then(|| {
            tokio::spawn(async move {
                let mut ticker = interval(Duration::from_secs(refresh_secs));
                ticker.tick().await; // Skip first immediate tick

                loop {
                    ticker.tick().await;

                    if done_clone.load(Ordering::Relaxed) {
                        break;
                    }

                    if let Err(e) = nubi.set_typing(&room_id_owned, true).await {
                        eprintln!("[nubi-agent] Failed to refresh typing: {}", e);
                    }
                }
            })
        });

        // Run the harness CLI
        let result = self.runtime.run(prompt, session).await;

        // Signal typing to stop
        done.store(true, Ordering::Relaxed);
        if let Some(handle) = typing_handle {
            handle.abort();
        }

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

    #[test]
    fn thread_rooms_name_their_parent_and_anchor() {
        assert_eq!(
            parse_thread_room("thread.pr-review-6ihc.540"),
            Some(("pr-review-6ihc".into(), 540))
        );
        assert_eq!(parse_thread_room("pr-review-6ihc"), None);
        assert_eq!(parse_thread_room("thread.x.notanumber"), None);
    }

    use crate::mandate::tests::{fixture, resolve_body};
    use crate::mandate::MandateSkill;
    use crate::mock_engine::{etag_for, MockEngine, MockState};
    use crate::runtime::StructuredReply;
    use crate::skills::SkillMeta;
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::atomic::AtomicUsize;
    use std::sync::RwLock;

    /// What the fake runtime was given, readable after it is boxed away.
    #[derive(Default)]
    struct FakeState {
        mandate: RwLock<Option<Mandate>>,
        /// Names of the ad-hoc skills last injected.
        adhoc: RwLock<String>,
        injections: AtomicUsize,
        runs: AtomicUsize,
        prompts: Mutex<Vec<String>>,
        sessions: Mutex<Vec<Option<Session>>>,
        /// Make `inject_mandate` fail, the way an unwritable prompt file does.
        fail_inject: AtomicBool,
        /// Hold every run until cleared, so a test can act between the
        /// mandate check and the run.
        block_runs: AtomicBool,
        /// Runs still to fail with an Err, then with is_error, then silence.
        fail_runs: AtomicUsize,
        error_runs: AtomicUsize,
        silent_runs: AtomicUsize,
        /// A structured reply to return instead of plain text.
        reply: RwLock<Option<StructuredReply>>,
        sessions_supported: AtomicBool,
    }

    impl FakeState {
        fn hash(&self) -> Option<String> {
            self.mandate
                .read()
                .unwrap()
                .as_ref()
                .map(Mandate::computed_hash)
        }

        /// Consume one unit of a countdown, if any is left.
        fn take(counter: &AtomicUsize) -> bool {
            let mut n = counter.load(Ordering::SeqCst);
            while n > 0 {
                match counter.compare_exchange(n, n - 1, Ordering::SeqCst, Ordering::SeqCst) {
                    Ok(_) => return true,
                    Err(actual) => n = actual,
                }
            }
            false
        }
    }

    struct FakeRuntime(Arc<FakeState>);

    #[async_trait]
    impl AgentRuntime for FakeRuntime {
        async fn check_available(&self) -> anyhow::Result<()> {
            Ok(())
        }
        fn build_prompt(&self, turn: &TurnInput) -> String {
            let mut prompt = String::new();
            for m in &turn.context {
                prompt.push_str(&format!(
                    "ctx #{} {}: {}\n",
                    m.global_index, m.sender, m.text
                ));
            }
            for t in &turn.triggers {
                prompt.push_str(&format!("new #{} {}: {}\n", t.index, t.sender, t.text));
            }
            if turn.resumed {
                prompt.push_str("(resumed)\n");
            }
            prompt
        }
        async fn run(&self, prompt: &str, session: Option<&Session>) -> anyhow::Result<RunOutput> {
            self.0.runs.fetch_add(1, Ordering::SeqCst);
            self.0.prompts.lock().unwrap().push(prompt.to_string());
            self.0.sessions.lock().unwrap().push(session.cloned());
            while self.0.block_runs.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            if FakeState::take(&self.0.fail_runs) {
                anyhow::bail!("claude failed (exit status: 1): boom");
            }
            // Prompt and hash from the same read, as CliRuntime does.
            let mandate_hash = self.0.hash();
            if FakeState::take(&self.0.error_runs) {
                return Ok(RunOutput {
                    is_error: true,
                    error: Some("error_during_execution (HTTP 529): Overloaded".into()),
                    mandate_hash,
                    num_turns: Some(1),
                    ..Default::default()
                });
            }
            if FakeState::take(&self.0.silent_runs) {
                return Ok(RunOutput {
                    mandate_hash,
                    num_turns: Some(1),
                    ..Default::default()
                });
            }
            let reply = self.0.reply.read().unwrap().clone();
            Ok(RunOutput {
                text: reply
                    .as_ref()
                    .map(|r| r.text.clone())
                    .unwrap_or_else(|| "Refunds take 30 days from purchase.".into()),
                reply,
                mandate_hash,
                num_turns: Some(2),
                session_id: session.map(|s| s.id.clone()),
                ..Default::default()
            })
        }
        fn supports_sessions(&self) -> bool {
            self.0.sessions_supported.load(Ordering::SeqCst)
        }
        fn supports_mcp(&self) -> bool {
            false
        }
        fn inject_mandate(&self, mandate: Option<&Mandate>, adhoc: &[Skill]) -> anyhow::Result<()> {
            if self.0.fail_inject.load(Ordering::SeqCst) {
                anyhow::bail!("prompt file is read-only");
            }
            *self.0.mandate.write().unwrap() = mandate.cloned();
            *self.0.adhoc.write().unwrap() = adhoc
                .iter()
                .map(|s| s.meta.name.clone())
                .collect::<Vec<_>>()
                .join(",");
            self.0.injections.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn mandate_hash(&self) -> Option<String> {
            self.0.hash()
        }
    }

    const ME: &str = "support-bot";

    fn hash_of(body: &serde_json::Value) -> Option<String> {
        body["hash"].as_str().map(String::from)
    }

    /// The fixture plus one more required skill: a different hash.
    fn grown_body() -> serde_json::Value {
        let mut skills = fixture();
        skills.push(MandateSkill {
            room_id: "escalation-2".into(),
            name: "Escalation".into(),
            from_set: None,
            version_index: 4,
            pinned: true,
            required: true,
            content: "Escalate refunds over $500.".into(),
        });
        resolve_body(&skills)
    }

    fn update_for(body: &serde_json::Value) -> String {
        json!({"agent_id": ME, "revision": 8, "hash": body["hash"]}).to_string()
    }

    fn stamped_hashes(engine: &MockEngine) -> Vec<Option<String>> {
        engine
            .state()
            .posts
            .iter()
            .map(|(_, body)| body["metadata"]["mandate_hash"].as_str().map(String::from))
            .collect()
    }

    fn posted_texts(engine: &MockEngine) -> Vec<String> {
        engine
            .state()
            .posts
            .iter()
            .map(|(_, body)| body["parts"][0]["text"].as_str().unwrap_or("").to_string())
            .collect()
    }

    fn temp_workspace() -> PathBuf {
        let ws = std::env::temp_dir().join(format!("nubi-agent-ws-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(ws.join(".nubi").join("rooms")).unwrap();
        ws
    }

    /// An agent against the mock engine, mandate fetched as `connect` does.
    async fn agent(engine: &MockEngine, config: AgentConfig) -> (Agent, Arc<FakeState>) {
        agent_with(engine, config, Vec::new(), true).await
    }

    async fn agent_with(
        engine: &MockEngine,
        config: AgentConfig,
        skills: Vec<Skill>,
        fetch_mandate: bool,
    ) -> (Agent, Arc<FakeState>) {
        let state = Arc::new(FakeState::default());
        let nubi = NubiClient::new(&engine.url, "tok", "s1", ME);
        let loader = MandateLoader::new(&engine.url, "tok", "s1", ME);
        let agent = Agent::assemble(
            nubi,
            AgentConfig {
                agent_id: ME.into(),
                rate_limit_ms: 0,
                ..config
            },
            Box::new(FakeRuntime(state.clone())),
            skills,
            Some(temp_workspace()),
            Some(loader),
        );
        if fetch_mandate {
            agent.shared.init_mandate().await;
        }
        (agent, state)
    }

    fn trigger() -> Trigger {
        trigger_at(10, "alice", "how long do refunds take?")
    }

    fn trigger_at(index: u64, sender: &str, text: &str) -> Trigger {
        Trigger {
            sender: sender.into(),
            text: text.into(),
            index,
            expect_reply: None,
            timestamp_ms: now_ms(),
        }
    }

    #[tokio::test]
    async fn replies_are_stamped_with_the_mandate_hash() {
        let body = resolve_body(&fixture());
        let engine = MockEngine::start(MockState {
            mandate: Some(body.clone()),
            ..Default::default()
        })
        .await;
        let (agent, fake) = agent(&engine, AgentConfig::default()).await;
        assert_eq!(
            fake.mandate.read().unwrap().as_ref().unwrap().skills.len(),
            2
        );

        agent.shared.reply("general", &[trigger()]).await.unwrap();

        let s = engine.state();
        assert_eq!(s.posts.len(), 1);
        assert_eq!(s.posts[0].1["metadata"]["mandate_hash"], body["hash"]);
        assert_eq!(s.posts[0].1["metadata"]["expect_reply"], json!([]));
        // Startup fetch, then a conditional check before the turn.
        assert_eq!(s.mandate_requests, vec![None, Some(etag_for(&body))]);
    }

    #[tokio::test]
    async fn an_engine_without_mandates_degrades_gracefully() {
        let engine = MockEngine::start(MockState::default()).await;
        let (agent, fake) = agent(&engine, AgentConfig::default()).await;
        assert!(fake.mandate.read().unwrap().is_none());

        agent.shared.reply("general", &[trigger()]).await.unwrap();

        let s = engine.state();
        assert_eq!(s.posts.len(), 1);
        assert!(s.posts[0].1["metadata"].get("mandate_hash").is_none());
    }

    #[tokio::test]
    async fn every_pending_message_reaches_the_prompt_and_the_journal_remembers() {
        let engine = MockEngine::start(MockState::default()).await;
        let (agent, fake) = agent(&engine, AgentConfig::default()).await;
        let triggers = vec![
            trigger_at(5, "alice", "first question"),
            trigger_at(6, "bob", "second question"),
        ];
        agent.shared.reply("general", &triggers).await.unwrap();

        let prompts = fake.prompts.lock().unwrap().clone();
        assert_eq!(prompts.len(), 1);
        assert!(prompts[0].contains("new #5 alice: first question"));
        assert!(prompts[0].contains("new #6 bob: second question"));

        let journal = agent.shared.journal_tail("general");
        assert_eq!(journal.len(), 1);
        assert!(journal[0].contains("#6 from bob"), "{journal:?}");
        assert!(journal[0].contains("replied: Refunds take 30 days"));
    }

    #[tokio::test]
    async fn a_structured_reply_carries_its_own_routing() {
        let engine = MockEngine::start(MockState::default()).await;
        let (agent, fake) = agent(&engine, AgentConfig::default()).await;
        *fake.reply.write().unwrap() = Some(StructuredReply {
            text: "pine, can you re-review?".into(),
            mentions: vec!["pine".into()],
            expect_reply: Some(vec!["pine".into()]),
            post: true,
        });
        agent.shared.reply("general", &[trigger()]).await.unwrap();
        {
            let s = engine.state();
            assert_eq!(s.posts[0].1["mentions"], json!(["pine"]));
            assert_eq!(s.posts[0].1["metadata"]["expect_reply"], json!(["pine"]));
        }

        // post: false means the answer already went out via tools.
        *fake.reply.write().unwrap() = Some(StructuredReply {
            text: "".into(),
            post: false,
            ..Default::default()
        });
        agent.shared.reply("general", &[trigger()]).await.unwrap();
        assert_eq!(engine.state().posts.len(), 1, "nothing more was posted");
        let journal = agent.shared.journal_tail("general");
        assert!(journal.last().unwrap().contains("answered via tools"));
    }

    #[tokio::test]
    async fn a_failed_run_is_retried_then_reported_to_the_room() {
        let engine = MockEngine::start(MockState::default()).await;
        let (agent, fake) = agent(&engine, AgentConfig::default()).await;

        // One transport failure, then success: the room sees only the answer.
        fake.fail_runs.store(1, Ordering::SeqCst);
        agent.shared.reply("general", &[trigger()]).await.unwrap();
        assert_eq!(fake.runs.load(Ordering::SeqCst), 2);
        assert_eq!(
            posted_texts(&engine),
            vec!["Refunds take 30 days from purchase."]
        );

        // Failing every time: the room is told, with the cause.
        fake.error_runs.store(10, Ordering::SeqCst);
        agent.shared.reply("general", &[trigger()]).await.unwrap();
        let texts = posted_texts(&engine);
        assert_eq!(texts.len(), 2);
        assert!(
            texts[1].starts_with(
                "Couldn't finish that one (error_during_execution (HTTP 529): Overloaded)"
            ),
            "{}",
            texts[1]
        );
        assert_eq!(
            fake.runs.load(Ordering::SeqCst),
            2 + MAX_RUNS_PER_TURN as usize
        );
        let journal = agent.shared.journal_tail("general");
        assert!(journal
            .last()
            .unwrap()
            .contains("failed: error_during_execution"));
    }

    #[tokio::test]
    async fn a_silent_run_is_nudged_once() {
        let engine = MockEngine::start(MockState::default()).await;
        let (agent, fake) = agent(&engine, AgentConfig::default()).await;
        fake.silent_runs.store(1, Ordering::SeqCst);
        agent.shared.reply("general", &[trigger()]).await.unwrap();
        let prompts = fake.prompts.lock().unwrap().clone();
        assert_eq!(prompts.len(), 2);
        assert!(!prompts[0].contains("previous attempt"));
        assert!(prompts[1].contains("previous attempt produced no reply"));
        assert_eq!(posted_texts(&engine).len(), 1);
    }

    #[tokio::test]
    async fn rooms_keep_a_session_and_resume_it_with_only_new_context() {
        let engine = MockEngine::start(MockState::default()).await;
        let (agent, fake) = agent(&engine, AgentConfig::default()).await;
        fake.sessions_supported.store(true, Ordering::SeqCst);

        // Room history: two old messages, then the trigger.
        {
            let mut buffer = agent.shared.buffer.lock().unwrap();
            for (i, text) in [(1u64, "hello"), (2, "anyone here?")] {
                buffer.push(
                    "general",
                    &crate::types::LogEntry {
                        global_index: i,
                        sender: "alice".into(),
                        timestamp_ms: now_ms(),
                        payload: crate::types::Payload {
                            payload_type: "message".into(),
                            parts: vec![crate::types::Part {
                                kind: "text".into(),
                                text: Some(text.into()),
                            }],
                            mentions: None,
                            metadata: None,
                        },
                    },
                );
            }
        }
        agent
            .shared
            .reply("general", &[trigger_at(3, "alice", "refunds?")])
            .await
            .unwrap();
        let first = fake.sessions.lock().unwrap()[0].clone().unwrap();
        assert!(!first.resume, "first turn starts a session");
        assert!(fake.prompts.lock().unwrap()[0].contains("ctx #1 alice: hello"));
        let saved = load_sessions(&sessions_path(agent.shared.workspace.as_ref().unwrap()));
        assert_eq!(saved["general"].id, first.id);
        assert_eq!(saved["general"].last_index, 3);

        // Next message in the room resumes it, and the old context is not resent.
        agent
            .shared
            .reply("general", &[trigger_at(4, "alice", "and exchanges?")])
            .await
            .unwrap();
        let second = fake.sessions.lock().unwrap()[1].clone().unwrap();
        assert!(second.resume);
        assert_eq!(second.id, first.id);
        let prompt = fake.prompts.lock().unwrap()[1].clone();
        assert!(!prompt.contains("ctx #1"), "{prompt}");
        assert!(prompt.contains("(resumed)"));

        // A resume that fails falls back to a fresh session in the same turn.
        fake.fail_runs.store(1, Ordering::SeqCst);
        agent
            .shared
            .reply("general", &[trigger_at(5, "alice", "still there?")])
            .await
            .unwrap();
        let sessions = fake.sessions.lock().unwrap().clone();
        assert_eq!(sessions.len(), 4);
        assert!(sessions[2].as_ref().unwrap().resume);
        let fresh = sessions[3].as_ref().unwrap();
        assert!(!fresh.resume);
        assert_ne!(fresh.id, first.id);
        assert_eq!(posted_texts(&engine).len(), 3, "every turn was answered");
    }

    #[tokio::test]
    async fn a_stale_hash_is_refetched_once_then_the_turn_is_refused() {
        // The engine's hash does not match the content it served.
        let mut body = resolve_body(&fixture());
        body["hash"] = json!("sha256:0000");
        let engine = MockEngine::start(MockState {
            mandate: Some(body.clone()),
            ..Default::default()
        })
        .await;
        let (agent, fake) = agent(&engine, AgentConfig::default()).await;

        agent.shared.reply("general", &[trigger()]).await.unwrap();

        let s = engine.state();
        assert!(s.posts.is_empty(), "a refused turn posts nothing");
        assert_eq!(fake.runs.load(Ordering::SeqCst), 0);
        // Startup, conditional check (304), one unconditional refetch.
        assert_eq!(s.mandate_requests, vec![None, Some(etag_for(&body)), None]);
    }

    #[tokio::test]
    async fn mandate_update_refetches_and_reinjects() {
        let old = resolve_body(&fixture());
        let engine = MockEngine::start(MockState {
            mandate: Some(old.clone()),
            ..Default::default()
        })
        .await;
        let (agent, fake) = agent(&engine, AgentConfig::default()).await;
        let injected = fake.injections.load(Ordering::SeqCst);

        // Someone else's update, and one for the hash we already hold, are no-ops.
        let other = json!({"agent_id": "other", "revision": 8, "hash": "sha256:ff"});
        agent.shared.on_mandate_update(&other.to_string()).await;
        let same = json!({"agent_id": ME, "revision": 7, "hash": old["hash"]});
        agent.shared.on_mandate_update(&same.to_string()).await;
        assert_eq!(engine.state().mandate_requests.len(), 1);

        let new = grown_body();
        engine.state().mandate = Some(new.clone());
        agent.shared.on_mandate_update(&update_for(&new)).await;

        assert_eq!(
            engine.state().mandate_requests,
            vec![None, Some(etag_for(&old))]
        );
        assert_eq!(fake.injections.load(Ordering::SeqCst), injected + 1);
        let m = fake.mandate.read().unwrap().clone().unwrap();
        assert_eq!(m.skills.len(), 3);
        assert_eq!(Some(m.hash), new["hash"].as_str().map(String::from));
    }

    #[tokio::test]
    async fn a_mandate_update_that_cannot_be_fetched_holds_replies() {
        let old = resolve_body(&fixture());
        let engine = MockEngine::start(MockState {
            mandate: Some(old.clone()),
            ..Default::default()
        })
        .await;
        let (agent, fake) = agent(&engine, AgentConfig::default()).await;
        let new = grown_body();

        engine.state().mandate_fail = Some(500);
        agent.shared.on_mandate_update(&update_for(&new)).await;
        agent.shared.reply("general", &[trigger()]).await.unwrap();
        assert!(engine.state().posts.is_empty());

        // The engine answers again but still serves the old mandate: the 304
        // for the old ETag does not settle the announced hash. One
        // unconditional refetch, then the turn is refused.
        engine.state().mandate_fail = None;
        let before = engine.state().mandate_requests.len();
        agent.shared.reply("general", &[trigger()]).await.unwrap();
        assert!(
            engine.state().posts.is_empty(),
            "old mandate is not current"
        );
        assert_eq!(fake.runs.load(Ordering::SeqCst), 0);
        assert_eq!(
            engine.state().mandate_requests[before..],
            [Some(etag_for(&old)), None]
        );
        assert!(agent.shared.mandate_status.lock().unwrap().dirty);

        // Once the engine serves the announced mandate, replies resume.
        engine.state().mandate = Some(new.clone());
        agent.shared.reply("general", &[trigger()]).await.unwrap();
        assert_eq!(stamped_hashes(&engine), vec![hash_of(&new)]);
        assert!(!agent.shared.mandate_status.lock().unwrap().dirty);
    }

    #[tokio::test]
    async fn a_stale_in_flight_fetch_does_not_clear_dirty() {
        let old = resolve_body(&fixture());
        let engine = MockEngine::start(MockState {
            mandate: Some(old.clone()),
            ..Default::default()
        })
        .await;
        let (agent, fake) = agent(&engine, AgentConfig::default()).await;
        let new = grown_body();

        // A check-before-turn fetch is in flight (the engine holds its reply)...
        engine.state().mandate_delay_ms = 300;
        let shared = Arc::clone(&agent.shared);
        let in_flight = tokio::spawn(async move { shared.refresh_mandate(true).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(engine.state().mandate_requests.len(), 2);

        // ...when the engine announces a new mandate.
        engine.state().mandate = Some(new.clone());
        engine.state().mandate_delay_ms = 0;
        let update: MandateUpdate = serde_json::from_str(&update_for(&new)).unwrap();
        assert!(agent.shared.announce_mandate(&update));

        // The stale fetch (a 304 for the old mandate) lands after the update.
        in_flight.await.unwrap().unwrap();
        let status = agent.shared.mandate_status.lock().unwrap().clone();
        assert!(
            status.dirty,
            "a fetch started before the update cannot settle it"
        );
        assert_eq!(status.announced, hash_of(&new));
        assert_eq!(fake.hash(), hash_of(&old));

        // The next turn fetches the announced mandate and is stamped with it.
        agent.shared.reply("general", &[trigger()]).await.unwrap();
        assert_eq!(stamped_hashes(&engine), vec![hash_of(&new)]);
        assert!(agent
            .shared
            .mandate_status
            .lock()
            .unwrap()
            .announced
            .is_none());
    }

    #[tokio::test]
    async fn a_failed_injection_keeps_the_old_mandate_and_refuses_the_turn() {
        let old = resolve_body(&fixture());
        let engine = MockEngine::start(MockState {
            mandate: Some(old.clone()),
            ..Default::default()
        })
        .await;
        let (agent, fake) = agent(&engine, AgentConfig::default()).await;
        let new = grown_body();

        // The prompt file cannot be rewritten when the new mandate arrives.
        fake.fail_inject.store(true, Ordering::SeqCst);
        engine.state().mandate = Some(new.clone());
        agent.shared.on_mandate_update(&update_for(&new)).await;
        assert_eq!(fake.hash(), hash_of(&old), "old mandate stays injected");
        assert!(agent.shared.mandate_status.lock().unwrap().dirty);

        // Not stamped with the new hash while the prompt still carries the old.
        agent.shared.reply("general", &[trigger()]).await.unwrap();
        assert!(engine.state().posts.is_empty());
        assert_eq!(fake.runs.load(Ordering::SeqCst), 0);
        assert_eq!(fake.hash(), hash_of(&old));

        // Once the rewrite works again, the next turn injects and answers.
        fake.fail_inject.store(false, Ordering::SeqCst);
        agent.shared.reply("general", &[trigger()]).await.unwrap();
        assert_eq!(fake.hash(), hash_of(&new));
        assert_eq!(stamped_hashes(&engine), vec![hash_of(&new)]);
    }

    #[tokio::test]
    async fn a_404_after_a_mandate_keeps_serving_the_last_good_one() {
        let old = resolve_body(&fixture());
        let engine = MockEngine::start(MockState {
            mandate: Some(old.clone()),
            ..Default::default()
        })
        .await;
        let (agent, fake) = agent(&engine, AgentConfig::default()).await;

        engine.state().mandate = None;
        agent.shared.reply("general", &[trigger()]).await.unwrap();

        assert_eq!(fake.hash(), hash_of(&old), "not un-injected");
        assert_eq!(stamped_hashes(&engine), vec![hash_of(&old)]);
        assert!(agent.shared.mandate_status.lock().unwrap().unverified);

        // The engine serving it again clears the doubt.
        engine.state().mandate = Some(old.clone());
        agent.shared.reply("general", &[trigger()]).await.unwrap();
        assert_eq!(engine.state().posts.len(), 2);
        assert!(!agent.shared.mandate_status.lock().unwrap().unverified);
    }

    #[tokio::test]
    async fn strict_mode_refuses_while_a_held_mandate_is_gone() {
        let old = resolve_body(&fixture());
        let engine = MockEngine::start(MockState {
            mandate: Some(old.clone()),
            ..Default::default()
        })
        .await;
        let config = AgentConfig {
            mandate_strict: true,
            ..Default::default()
        };
        let (agent, fake) = agent(&engine, config).await;

        engine.state().mandate = None;
        agent.shared.reply("general", &[trigger()]).await.unwrap();
        assert!(engine.state().posts.is_empty());
        assert_eq!(fake.runs.load(Ordering::SeqCst), 0);
        assert_eq!(fake.hash(), hash_of(&old), "kept, not wiped");

        engine.state().mandate = Some(old.clone());
        agent.shared.reply("general", &[trigger()]).await.unwrap();
        assert_eq!(stamped_hashes(&engine), vec![hash_of(&old)]);
    }

    #[tokio::test]
    async fn an_unparseable_body_keeps_the_last_good_mandate() {
        let old = resolve_body(&fixture());
        let engine = MockEngine::start(MockState {
            mandate: Some(old.clone()),
            ..Default::default()
        })
        .await;
        let (agent, fake) = agent(&engine, AgentConfig::default()).await;

        // 200 with a body missing `hash`: a 304 is impossible (new ETag) so
        // every check sees the broken body.
        engine.state().mandate = Some(json!({"agent_id": ME, "skills": []}));
        agent.shared.reply("general", &[trigger()]).await.unwrap();

        assert_eq!(fake.hash(), hash_of(&old));
        assert_eq!(stamped_hashes(&engine), vec![hash_of(&old)]);
        assert!(agent.shared.mandate_status.lock().unwrap().unverified);
    }

    #[tokio::test]
    async fn strict_mode_refuses_an_unparseable_body() {
        let old = resolve_body(&fixture());
        let engine = MockEngine::start(MockState {
            mandate: Some(old.clone()),
            ..Default::default()
        })
        .await;
        let config = AgentConfig {
            mandate_strict: true,
            ..Default::default()
        };
        let (agent, fake) = agent(&engine, config).await;

        engine.state().mandate = Some(json!({"agent_id": ME, "skills": []}));
        agent.shared.reply("general", &[trigger()]).await.unwrap();
        assert!(engine.state().posts.is_empty());
        assert_eq!(fake.runs.load(Ordering::SeqCst), 0);
        assert_eq!(
            fake.hash(),
            hash_of(&old),
            "kept for when the engine recovers"
        );

        engine.state().mandate = Some(old.clone());
        agent.shared.reply("general", &[trigger()]).await.unwrap();
        assert_eq!(stamped_hashes(&engine), vec![hash_of(&old)]);
    }

    #[tokio::test]
    async fn an_unparseable_body_with_no_mandate_yet_is_not_fatal_unless_strict() {
        let engine = MockEngine::start(MockState {
            mandate: Some(json!({"agent_id": ME, "skills": []})),
            ..Default::default()
        })
        .await;
        for strict in [false, true] {
            let config = AgentConfig {
                mandate_strict: strict,
                ..Default::default()
            };
            // No init_mandate: it would back off (and in strict mode never stop).
            let (agent, state) = agent_with(&engine, config, Vec::new(), false).await;
            agent.shared.reply("general", &[trigger()]).await.unwrap();
            assert!(state.mandate.read().unwrap().is_none());
        }
        // Only the lenient agent answered, and without a stamp.
        assert_eq!(stamped_hashes(&engine), vec![None]);
    }

    #[tokio::test]
    async fn a_reply_is_stamped_with_the_hash_of_the_prompt_it_ran_with() {
        let old = resolve_body(&fixture());
        let engine = MockEngine::start(MockState {
            mandate: Some(old.clone()),
            ..Default::default()
        })
        .await;
        let (agent, fake) = agent(&engine, AgentConfig::default()).await;
        let new = grown_body();

        // The turn passes its check with the old mandate and starts running...
        fake.block_runs.store(true, Ordering::SeqCst);
        let shared = Arc::clone(&agent.shared);
        let turn = tokio::spawn(async move { shared.reply("general", &[trigger()]).await });
        while fake.runs.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // ...a refresh swaps the mandate under it before the run reads the prompt.
        engine.state().mandate = Some(new.clone());
        agent.shared.refresh_mandate(false).await.unwrap();
        assert_eq!(fake.hash(), hash_of(&new));
        fake.block_runs.store(false, Ordering::SeqCst);
        turn.await.unwrap().unwrap();

        // Stamped with what the prompt carried when it ran, not what the check saw.
        assert_eq!(stamped_hashes(&engine), vec![hash_of(&new)]);
    }

    #[tokio::test]
    async fn strict_mode_refuses_while_no_mandate_was_ever_fetched() {
        let engine = MockEngine::start(MockState {
            mandate_fail: Some(503),
            ..Default::default()
        })
        .await;
        let config = AgentConfig {
            mandate_strict: true,
            ..Default::default()
        };
        // No init_mandate: it would retry forever in strict mode.
        let (agent, _) = agent_with(&engine, config, Vec::new(), false).await;
        agent.shared.reply("general", &[trigger()]).await.unwrap();
        assert!(engine.state().posts.is_empty());

        // Without strict, the same outage answers without a mandate.
        let (lenient, _) = agent_with(&engine, AgentConfig::default(), Vec::new(), false).await;
        lenient.shared.reply("general", &[trigger()]).await.unwrap();
        assert_eq!(engine.state().posts.len(), 1);
    }

    fn local(name: &str) -> Skill {
        Skill {
            meta: SkillMeta {
                name: name.into(),
                enabled: true,
                ..Default::default()
            },
            content: format!("{name} from a file"),
            path: PathBuf::from(format!("/skills/{name}.md")),
        }
    }

    fn room(id: &str, name: &str) -> Skill {
        Skill {
            path: PathBuf::from(format!("room://{id}")),
            ..local(name)
        }
    }

    fn names(skills: &[Skill]) -> Vec<String> {
        skills.iter().map(|s| s.meta.name.clone()).collect()
    }

    #[test]
    fn adhoc_skills_colliding_with_the_mandate_are_dropped() {
        let mandate: Mandate = serde_json::from_value(resolve_body(&fixture())).unwrap();
        let skills = vec![
            room("refund-9x2k", "Old refund name"), // same room id
            local("Tone"),                          // same name as a mandated skill
            local(" tone "),                        // same name, other case and padding
            local("deploys"),
            room("faq-1", "FAQ"),
        ];

        let kept = names(&adhoc_skills(&skills, Some(&mandate), true));
        assert_eq!(kept, vec!["deploys", "FAQ"]);

        // mandate_only / --no-local-skills: files are skipped entirely.
        let kept = names(&adhoc_skills(&skills, Some(&mandate), false));
        assert_eq!(kept, vec!["FAQ"]);

        // No mandate: everything stays.
        assert_eq!(adhoc_skills(&skills, None, true).len(), 5);
    }

    #[tokio::test]
    async fn mandate_only_policy_drops_local_skills() {
        let mut body = resolve_body(&fixture());
        body["policy"]["skills_policy"] = json!("mandate_only");
        let engine = MockEngine::start(MockState {
            mandate: Some(body),
            ..Default::default()
        })
        .await;
        let (agent, state) = agent_with(
            &engine,
            AgentConfig::default(),
            vec![local("deploys"), room("faq-1", "FAQ")],
            false,
        )
        .await;
        assert!(state.adhoc.read().unwrap().contains("deploys"));

        agent.shared.init_mandate().await;
        let adhoc = state.adhoc.read().unwrap().clone();
        assert!(!adhoc.contains("deploys"));
        assert!(adhoc.contains("FAQ"));
    }
}
