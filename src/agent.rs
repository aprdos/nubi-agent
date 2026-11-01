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
use crate::harness::{CliRuntime, HarnessKind, McpServer};
use crate::mandate::{FetchError, Fetched, Mandate, MandateLoader};
use crate::nubi::NubiClient;
use crate::runtime::{AgentRuntime, RunOutput};
use crate::skills::{CompositeSkillLoader, RoomSkillLoader, Skill, SkillLoader};
use crate::types::{MandateUpdate, SseData};

/// How long a room's incoming burst is allowed to settle before we pick a
/// message to answer. Collapses a backlog into one reply to the newest message.
const COALESCE_WINDOW_MS: u64 = 500;

/// How often pending read offsets are pushed to the server.
const OFFSET_FLUSH_SECS: u64 = 2;

/// Ceiling on simultaneously running harness processes across all rooms.
const MAX_CONCURRENT_REPLIES: usize = 4;

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
    pub heartbeat_interval_secs: u64,
    pub llm_base_url: Option<String>,
    pub llm_api_key: Option<String>,
    pub llm_model: Option<String>,
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
            heartbeat_interval_secs: 30,
            llm_base_url: None,
            llm_api_key: None,
            llm_model: None,
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
    /// Room skill loader for cache invalidation on SSE updates.
    #[allow(dead_code)]
    room_skill_loader: Option<RoomSkillLoader>,
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
    /// Loaded skills for this agent
    #[allow(dead_code)]
    skills: Vec<Skill>,
}

impl Agent {
    /// Create a new agent with synchronous filesystem-only skill loading.
    /// Use `connect` for full room-based skill loading.
    pub fn new(nubi: NubiClient, config: AgentConfig) -> Self {
        let mcp = config.mcp_url.as_ref().map(|url| McpServer {
            url: url.clone(),
            token: nubi.token().to_string(),
        });

        let memory_file = config.harness.harness().memory_file();
        let workspace = Self::prepare_workspace(&config.agent_id, memory_file);

        // Load skills from filesystem only (sync)
        let (skills, _) = Self::load_skills_sync(&config.agent_id);

        let runtime = CliRuntime::new(config.harness, &config.agent_id)
            .with_skip_permissions(config.skip_permissions)
            .with_system_prompt(config.system_prompt.clone())
            .with_mcp(mcp)
            .with_workspace(workspace)
            .with_llm_base_url(config.llm_base_url.clone())
            .with_llm_api_key(config.llm_api_key.clone())
            .with_model(config.llm_model.clone());

        Self::assemble(nubi, config, Box::new(runtime), skills, None, None)
    }

    /// Create a new agent with async room-based skill loading.
    /// Falls back to filesystem if room API fails.
    pub async fn connect(nubi: NubiClient, config: AgentConfig) -> Self {
        let mcp = config.mcp_url.as_ref().map(|url| McpServer {
            url: url.clone(),
            token: nubi.token().to_string(),
        });

        let memory_file = config.harness.harness().memory_file();
        let workspace = Self::prepare_workspace(&config.agent_id, memory_file);

        // Load skills with room-based loader (async)
        let (skills, room_skill_loader) = Self::load_skills_async(&config.agent_id, &nubi).await;

        let runtime = CliRuntime::new(config.harness, &config.agent_id)
            .with_skip_permissions(config.skip_permissions)
            .with_system_prompt(config.system_prompt.clone())
            .with_mcp(mcp)
            .with_workspace(workspace)
            .with_llm_base_url(config.llm_base_url.clone())
            .with_llm_api_key(config.llm_api_key.clone())
            .with_model(config.llm_model.clone());

        let mandate =
            MandateLoader::new(nubi.base_url(), nubi.token(), nubi.space(), nubi.agent_id());

        let agent = Self::assemble(
            nubi,
            config,
            Box::new(runtime),
            skills,
            room_skill_loader,
            Some(mandate),
        );
        agent.shared.init_mandate().await;
        agent
    }

    /// Wire the shared state together and inject the ad-hoc skills.
    fn assemble(
        nubi: NubiClient,
        config: AgentConfig,
        runtime: Box<dyn AgentRuntime>,
        skills: Vec<Skill>,
        room_skill_loader: Option<RoomSkillLoader>,
        mandate: Option<MandateLoader>,
    ) -> Self {
        let buffer = RoomBuffer::new()
            .with_context_size(config.context_size)
            .with_rate_limit_ms(config.rate_limit_ms);

        let start_time_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

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
            room_skill_loader,
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
            skills,
        }
    }

    /// Load skills from filesystem only (synchronous).
    fn load_skills_sync(agent_id: &str) -> (Vec<Skill>, String) {
        let home = match dirs::home_dir() {
            Some(h) => h,
            None => return (Vec::new(), String::new()),
        };

        // Validate agent_id to prevent path traversal (CWE-22)
        if !is_safe_path_segment(agent_id) {
            eprintln!(
                "[nubi-agent] Invalid agent_id for skills path: {}",
                agent_id
            );
            return (Vec::new(), String::new());
        }

        let shared_dir = home.join(".nubi").join("skills");
        let agent_skills_dir = home.join(".nubi-agents").join(agent_id).join("skills");

        // Create directories if they don't exist (for discoverability)
        let _ = fs::create_dir_all(&shared_dir);
        let _ = fs::create_dir_all(&agent_skills_dir);

        let loader = SkillLoader::new(shared_dir, Some(agent_skills_dir));
        let skills = loader.load_all();
        let prompt = SkillLoader::build_prompt_section(&skills);

        (skills, prompt)
    }

    /// Load skills with room-based loader, falling back to filesystem.
    async fn load_skills_async(
        agent_id: &str,
        nubi: &NubiClient,
    ) -> (Vec<Skill>, Option<RoomSkillLoader>) {
        let home = match dirs::home_dir() {
            Some(h) => h,
            None => return (Vec::new(), None),
        };

        // Validate agent_id to prevent path traversal (CWE-22)
        if !is_safe_path_segment(agent_id) {
            eprintln!(
                "[nubi-agent] Invalid agent_id for skills path: {}",
                agent_id
            );
            return (Vec::new(), None);
        }

        let shared_dir = home.join(".nubi").join("skills");
        let agent_skills_dir = home.join(".nubi-agents").join(agent_id).join("skills");

        // Create directories if they don't exist (for discoverability)
        let _ = fs::create_dir_all(&shared_dir);
        let _ = fs::create_dir_all(&agent_skills_dir);

        // Create loaders. Filesystem skills are always read; whether they are
        // injected is decided against the mandate in `adhoc_section`.
        let fs_loader = SkillLoader::new(shared_dir, Some(agent_skills_dir));
        let room_loader = RoomSkillLoader::new(nubi.base_url(), nubi.token(), nubi.space());

        let composite = CompositeSkillLoader::new(Some(room_loader), fs_loader);
        let skills = composite.load_all().await;

        // Extract room loader with cache intact for SSE invalidation
        let room_loader = composite.into_room_loader();

        (skills, room_loader)
    }

    /// Load skills from shared and per-agent directories (legacy, kept for compatibility)
    #[allow(dead_code)]
    fn load_skills(agent_id: &str) -> (Vec<Skill>, String) {
        Self::load_skills_sync(agent_id)
    }

    /// A stable per-agent directory for the spawned CLI to run in.
    ///
    /// Each message is a one-shot run that remembers nothing, so the directory
    /// is the only thing that survives: the harness's memory file in it
    /// (CLAUDE.md, AGENTS.md, GEMINI.md) is reloaded on every run. Agents get
    /// their own so one cannot read another's notes.
    ///
    /// Returns None if there is no home directory to anchor to, which just
    /// leaves the CLI in the launching cwd as before.
    fn prepare_workspace(agent_id: &str, memory_file: &str) -> Option<PathBuf> {
        // Validate agent_id to prevent path traversal (CWE-22)
        if !is_safe_path_segment(agent_id) {
            eprintln!("[nubi-agent] Invalid agent_id for workspace: {}", agent_id);
            return None;
        }

        let path = dirs::home_dir()?.join(".nubi-agents").join(agent_id);
        if let Err(e) = fs::create_dir_all(&path) {
            eprintln!("[nubi-agent] Cannot create workspace {:?}: {}", path, e);
            return None;
        }
        Self::seed_memory_file(&path, memory_file, agent_id);
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

/// The ad-hoc skills section: skills from room membership and files, minus
/// any that collide with a mandated skill by room id or name (so a local file
/// cannot shadow a mandate), and minus files when local skills are off.
fn adhoc_section(skills: &[Skill], mandate: Option<&Mandate>, allow_local: bool) -> String {
    let mandated = |skill: &Skill| {
        mandate.is_some_and(|m| {
            m.skills.iter().any(|ms| {
                // Room ids are engine identifiers; names are typed by people
                skill.room_id() == Some(ms.room_id.as_str())
                    || skill.meta.name.trim().eq_ignore_ascii_case(ms.name.trim())
            })
        })
    };
    let kept: Vec<Skill> = skills
        .iter()
        .filter(|s| allow_local || s.room_id().is_some())
        .filter(|s| !mandated(s))
        .cloned()
        .collect();
    SkillLoader::build_prompt_section(&kept)
}

impl Shared {
    /// Inject the cached mandate and the ad-hoc skills that survive it.
    fn apply_mandate(&self) -> anyhow::Result<()> {
        let mandate = self.mandate.as_ref().and_then(MandateLoader::current);
        let allow_local =
            !self.config.no_local_skills && !mandate.as_ref().is_some_and(Mandate::is_mandate_only);
        let adhoc = adhoc_section(&self.adhoc_skills, mandate.as_ref(), allow_local);
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

        if let Err(reason) = self.check_mandate().await {
            eprintln!("[nubi-agent] Refusing to answer in {}: {}", room_id, reason);
            return Ok(());
        }

        eprintln!(
            "[nubi-agent] @{} in {}: {}",
            self.config.agent_id,
            room_id,
            trigger.text.chars().take(50).collect::<String>()
        );

        self.prime_buffer(room_id).await;

        let context = self.buffer.lock().unwrap().context(room_id);
        let prompt = self
            .runtime
            .build_prompt(room_id, &trigger.sender, &trigger.text, &context);

        eprintln!("[nubi-agent] Running {:?}...", self.config.harness);
        let output = self.run_with_typing(room_id, &prompt).await?;
        // Stamp what the prompt really carried, not what the check saw: a
        // refresh can land between the two.
        let (response, mandate_hash) = (output.text, output.mandate_hash);

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
        self.nubi
            .post(room_id, &response, mandate_hash.as_deref())
            .await?;
        self.buffer.lock().unwrap().mark_responded(room_id);

        Ok(())
    }

    /// Run the harness with typing indicator instead of heartbeat messages
    async fn run_with_typing(&self, room_id: &str, prompt: &str) -> anyhow::Result<RunOutput> {
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

        // Run the harness CLI
        let result = self.runtime.run(prompt).await;

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

    use crate::buffer::BufferedMessage;
    use crate::mandate::tests::{fixture, resolve_body};
    use crate::mandate::MandateSkill;
    use crate::mock_engine::{etag_for, MockEngine, MockState};
    use crate::skills::SkillMeta;
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::RwLock;

    /// What the fake runtime was given, readable after it is boxed away.
    #[derive(Default)]
    struct FakeState {
        mandate: RwLock<Option<Mandate>>,
        adhoc: RwLock<String>,
        injections: std::sync::atomic::AtomicUsize,
        runs: std::sync::atomic::AtomicUsize,
        /// Make `inject_mandate` fail, the way an unwritable prompt file does.
        fail_inject: AtomicBool,
        /// Hold every run until cleared, so a test can act between the
        /// mandate check and the run.
        block_runs: AtomicBool,
    }

    impl FakeState {
        fn hash(&self) -> Option<String> {
            self.mandate
                .read()
                .unwrap()
                .as_ref()
                .map(Mandate::computed_hash)
        }
    }

    struct FakeRuntime(Arc<FakeState>);

    #[async_trait]
    impl AgentRuntime for FakeRuntime {
        async fn check_available(&self) -> anyhow::Result<()> {
            Ok(())
        }
        fn build_prompt(&self, _: &str, _: &str, message: &str, _: &[BufferedMessage]) -> String {
            message.to_string()
        }
        async fn run(&self, _prompt: &str) -> anyhow::Result<RunOutput> {
            self.0.runs.fetch_add(1, Ordering::SeqCst);
            while self.0.block_runs.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            // Prompt and hash from the same read, as CliRuntime does.
            let mandate_hash = self.0.hash();
            Ok(RunOutput {
                text: "Refunds take 30 days from purchase.".into(),
                mandate_hash,
            })
        }
        fn supports_mcp(&self) -> bool {
            false
        }
        fn inject_context(&mut self, _: &str) {}
        fn inject_mandate(&self, mandate: Option<&Mandate>, adhoc: &str) -> anyhow::Result<()> {
            if self.0.fail_inject.load(Ordering::SeqCst) {
                anyhow::bail!("prompt file is read-only");
            }
            *self.0.mandate.write().unwrap() = mandate.cloned();
            *self.0.adhoc.write().unwrap() = adhoc.to_string();
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

    /// An agent against the mock engine, mandate fetched as `connect` does.
    async fn agent(engine: &MockEngine, config: AgentConfig) -> (Agent, Arc<FakeState>) {
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
            Vec::new(),
            None,
            Some(loader),
        );
        agent.shared.init_mandate().await;
        (agent, state)
    }

    fn trigger() -> Trigger {
        Trigger {
            sender: "alice".into(),
            text: "how long do refunds take?".into(),
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

        agent.shared.reply("general", &trigger()).await.unwrap();

        let s = engine.state();
        assert_eq!(s.posts.len(), 1);
        assert_eq!(s.posts[0].1["metadata"]["mandate_hash"], body["hash"]);
        // Startup fetch, then a conditional check before the turn.
        assert_eq!(s.mandate_requests, vec![None, Some(etag_for(&body))]);
    }

    #[tokio::test]
    async fn an_engine_without_mandates_degrades_gracefully() {
        let engine = MockEngine::start(MockState::default()).await;
        let (agent, fake) = agent(&engine, AgentConfig::default()).await;
        assert!(fake.mandate.read().unwrap().is_none());

        agent.shared.reply("general", &trigger()).await.unwrap();

        let s = engine.state();
        assert_eq!(s.posts.len(), 1);
        assert!(s.posts[0].1["metadata"].get("mandate_hash").is_none());
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

        agent.shared.reply("general", &trigger()).await.unwrap();

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
        agent.shared.reply("general", &trigger()).await.unwrap();
        assert!(engine.state().posts.is_empty());

        // The engine answers again but still serves the old mandate: the 304
        // for the old ETag does not settle the announced hash. One
        // unconditional refetch, then the turn is refused.
        engine.state().mandate_fail = None;
        let before = engine.state().mandate_requests.len();
        agent.shared.reply("general", &trigger()).await.unwrap();
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
        agent.shared.reply("general", &trigger()).await.unwrap();
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
        agent.shared.reply("general", &trigger()).await.unwrap();
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
        agent.shared.reply("general", &trigger()).await.unwrap();
        assert!(engine.state().posts.is_empty());
        assert_eq!(fake.runs.load(Ordering::SeqCst), 0);
        assert_eq!(fake.hash(), hash_of(&old));

        // Once the rewrite works again, the next turn injects and answers.
        fake.fail_inject.store(false, Ordering::SeqCst);
        agent.shared.reply("general", &trigger()).await.unwrap();
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
        agent.shared.reply("general", &trigger()).await.unwrap();

        assert_eq!(fake.hash(), hash_of(&old), "not un-injected");
        assert_eq!(stamped_hashes(&engine), vec![hash_of(&old)]);
        assert!(agent.shared.mandate_status.lock().unwrap().unverified);

        // The engine serving it again clears the doubt.
        engine.state().mandate = Some(old.clone());
        agent.shared.reply("general", &trigger()).await.unwrap();
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
        agent.shared.reply("general", &trigger()).await.unwrap();
        assert!(engine.state().posts.is_empty());
        assert_eq!(fake.runs.load(Ordering::SeqCst), 0);
        assert_eq!(fake.hash(), hash_of(&old), "kept, not wiped");

        engine.state().mandate = Some(old.clone());
        agent.shared.reply("general", &trigger()).await.unwrap();
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
        agent.shared.reply("general", &trigger()).await.unwrap();

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
        agent.shared.reply("general", &trigger()).await.unwrap();
        assert!(engine.state().posts.is_empty());
        assert_eq!(fake.runs.load(Ordering::SeqCst), 0);
        assert_eq!(
            fake.hash(),
            hash_of(&old),
            "kept for when the engine recovers"
        );

        engine.state().mandate = Some(old.clone());
        agent.shared.reply("general", &trigger()).await.unwrap();
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
            let state = Arc::new(FakeState::default());
            let agent = Agent::assemble(
                NubiClient::new(&engine.url, "tok", "s1", ME),
                AgentConfig {
                    agent_id: ME.into(),
                    mandate_strict: strict,
                    ..Default::default()
                },
                Box::new(FakeRuntime(state.clone())),
                Vec::new(),
                None,
                Some(MandateLoader::new(&engine.url, "tok", "s1", ME)),
            );
            // No init_mandate: it would back off (and in strict mode never stop).
            agent.shared.reply("general", &trigger()).await.unwrap();
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
        let turn = tokio::spawn(async move { shared.reply("general", &trigger()).await });
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
        let state = Arc::new(FakeState::default());
        let agent = Agent::assemble(
            NubiClient::new(&engine.url, "tok", "s1", ME),
            AgentConfig {
                agent_id: ME.into(),
                mandate_strict: true,
                ..Default::default()
            },
            Box::new(FakeRuntime(state)),
            Vec::new(),
            None,
            Some(MandateLoader::new(&engine.url, "tok", "s1", ME)),
        );
        // No init_mandate: it would retry forever in strict mode.
        agent.shared.reply("general", &trigger()).await.unwrap();
        assert!(engine.state().posts.is_empty());

        // Without strict, the same outage answers without a mandate.
        let (lenient, _) = {
            let state = Arc::new(FakeState::default());
            let a = Agent::assemble(
                NubiClient::new(&engine.url, "tok", "s1", ME),
                AgentConfig {
                    agent_id: ME.into(),
                    ..Default::default()
                },
                Box::new(FakeRuntime(state.clone())),
                Vec::new(),
                None,
                Some(MandateLoader::new(&engine.url, "tok", "s1", ME)),
            );
            (a, state)
        };
        lenient.shared.reply("general", &trigger()).await.unwrap();
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

        let section = adhoc_section(&skills, Some(&mandate), true);
        assert!(!section.contains("Old refund name"));
        assert!(!section.contains("### Tone"));
        assert!(!section.contains("tone from a file"));
        assert!(section.contains("### deploys"));
        assert!(section.contains("### FAQ"));

        // mandate_only / --no-local-skills: files are skipped entirely.
        let section = adhoc_section(&skills, Some(&mandate), false);
        assert!(!section.contains("deploys"));
        assert!(section.contains("### FAQ"));

        // No mandate: everything stays.
        let section = adhoc_section(&skills, None, true);
        assert!(section.contains("Old refund name") && section.contains("### Tone"));
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
        let state = Arc::new(FakeState::default());
        let agent = Agent::assemble(
            NubiClient::new(&engine.url, "tok", "s1", ME),
            AgentConfig {
                agent_id: ME.into(),
                ..Default::default()
            },
            Box::new(FakeRuntime(state.clone())),
            vec![local("deploys"), room("faq-1", "FAQ")],
            None,
            Some(MandateLoader::new(&engine.url, "tok", "s1", ME)),
        );
        assert!(state.adhoc.read().unwrap().contains("deploys"));

        agent.shared.init_mandate().await;
        let adhoc = state.adhoc.read().unwrap().clone();
        assert!(!adhoc.contains("deploys"));
        assert!(adhoc.contains("### FAQ"));
    }
}
