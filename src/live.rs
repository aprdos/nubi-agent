//! A CLI session that stays up between messages (Claude Code with
//! `--input-format stream-json`).
//!
//! The per-turn mode starts `claude -p` for every message and ends it with
//! the reply, so anything the agent started in the background dies with the
//! turn. Here one process per room stays up: messages are written to its
//! stdin, one `result` event ends each turn, and when a background command
//! the agent started finishes, the CLI starts a turn of its own.
//!
//! The agent answers with nubi_send (the CLI's final text is never posted),
//! so turns are only told apart, not parsed for a reply: a turn begins with
//! the echo of a message we wrote (`--replay-user-messages`), or with no echo
//! at all when the CLI started it. The latter come out of
//! [`LiveSession::spontaneous`].

use std::collections::{HashSet, VecDeque};
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};

/// The tool a live session answers people with.
pub const SEND_TOOL: &str = "mcp__nubi__nubi_send";

/// How to start a live session for a room.
#[derive(Debug, Clone)]
pub struct LiveCommand {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(OsString, OsString)>,
    pub cwd: Option<PathBuf>,
    /// Changes with the system prompt (mandate, skills): a session started
    /// with another one is restarted before its next turn.
    pub prompt_key: u64,
    pub mandate_hash: Option<String>,
}

/// One finished turn.
#[derive(Debug, Clone, Default)]
pub struct LiveTurn {
    /// The `result` event that ended it; Null when the session ended first.
    pub result: Value,
    /// Rooms it sent a message to with nubi_send, in order.
    pub sent_to: Vec<String>,
    /// Started by the CLI itself (a background command finished), not by a
    /// message of ours.
    pub spontaneous: bool,
}

impl LiveTurn {
    pub fn answered(&self, room_id: &str) -> bool {
        self.sent_to.iter().any(|r| r == room_id)
    }
}

/// What one stdout line says, as far as turns go.
#[derive(Debug, PartialEq)]
enum Event {
    /// A message we wrote, taken up by the session.
    Echo,
    /// The model is working (text, a tool call, a tool result).
    Activity,
    /// A nubi_send to this room.
    Sent(String),
    BackgroundStarted(String),
    BackgroundEnded(String),
    Result(Value),
    Ignore,
}

fn parse_line(line: &str) -> Vec<Event> {
    let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
        return vec![Event::Ignore];
    };
    match v["type"].as_str() {
        Some("user") if v["isReplay"].as_bool() == Some(true) => vec![Event::Echo],
        Some("user") => vec![Event::Activity],
        Some("assistant") => {
            let mut out = vec![Event::Activity];
            for block in v["message"]["content"].as_array().into_iter().flatten() {
                if block["type"] == "tool_use" && block["name"] == SEND_TOOL {
                    if let Some(room) = block["input"]["room_id"].as_str() {
                        out.push(Event::Sent(room.to_string()));
                    }
                }
            }
            out
        }
        Some("system") => match v["subtype"].as_str() {
            Some("task_started") => match v["task_id"].as_str() {
                Some(id) => vec![Event::BackgroundStarted(id.to_string())],
                None => vec![Event::Ignore],
            },
            Some("task_notification") => match v["task_id"].as_str() {
                Some(id) => vec![Event::BackgroundEnded(id.to_string())],
                None => vec![Event::Ignore],
            },
            _ => vec![Event::Ignore],
        },
        Some("result") => vec![Event::Result(v)],
        _ => vec![Event::Ignore],
    }
}

#[derive(Default)]
struct Current {
    owner: Option<oneshot::Sender<LiveTurn>>,
    sent_to: Vec<String>,
}

#[derive(Default)]
struct State {
    /// Messages written and not yet taken up, oldest first.
    waiting: VecDeque<oneshot::Sender<LiveTurn>>,
    /// The turn in progress, if any.
    current: Option<Current>,
    background: HashSet<String>,
    last_activity: Option<Instant>,
    ended: bool,
}

impl State {
    fn take_up(&mut self) {
        let owner = self.waiting.pop_front();
        match self.current.as_mut() {
            // The CLI started a turn and our message was folded into it.
            Some(c) if c.owner.is_none() => c.owner = owner,
            Some(_) => {
                if let Some(o) = owner {
                    self.waiting.push_front(o);
                }
            }
            None => {
                self.current = Some(Current {
                    owner,
                    sent_to: Vec::new(),
                })
            }
        }
    }

    fn apply(&mut self, event: Event, spontaneous: &mpsc::UnboundedSender<LiveTurn>) {
        if event != Event::Ignore {
            self.last_activity = Some(Instant::now());
        }
        match event {
            Event::Echo => self.take_up(),
            Event::Activity => {
                self.current.get_or_insert_with(Current::default);
            }
            Event::Sent(room) => self
                .current
                .get_or_insert_with(Current::default)
                .sent_to
                .push(room),
            Event::BackgroundStarted(id) => {
                self.background.insert(id);
            }
            Event::BackgroundEnded(id) => {
                self.background.remove(&id);
            }
            Event::Result(result) => {
                let cur = self.current.take().unwrap_or_default();
                let turn = LiveTurn {
                    result,
                    sent_to: cur.sent_to,
                    spontaneous: cur.owner.is_none(),
                };
                match cur.owner {
                    Some(owner) => {
                        let _ = owner.send(turn);
                    }
                    None => {
                        let _ = spontaneous.send(turn);
                    }
                }
            }
            Event::Ignore => {}
        }
    }

    /// The session is gone: whoever is waiting gets nothing back.
    fn end(&mut self) {
        self.ended = true;
        self.waiting.clear();
        self.current = None;
        self.background.clear();
    }
}

/// One room's session.
pub struct LiveSession {
    stdin: tokio::sync::Mutex<Box<dyn AsyncWrite + Send + Unpin>>,
    state: Arc<Mutex<State>>,
    child: Mutex<Option<Child>>,
    pub prompt_key: u64,
    pub mandate_hash: Option<String>,
    started: Instant,
}

impl LiveSession {
    /// Start the CLI. Turns it starts on its own go to `spontaneous`.
    pub fn spawn(
        cmd: &LiveCommand,
        spontaneous: mpsc::UnboundedSender<LiveTurn>,
    ) -> anyhow::Result<Arc<Self>> {
        let mut c = Command::new(&cmd.program);
        c.args(&cmd.args)
            .env_clear()
            .envs(cmd.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(unix)]
        c.process_group(0);
        if let Some(dir) = &cmd.cwd {
            c.current_dir(dir);
        }
        let mut child = c.spawn()?;
        let stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let session = Self::from_io(
            stdout,
            stdin,
            cmd.prompt_key,
            cmd.mandate_hash.clone(),
            spontaneous,
        );
        *session.child.lock().unwrap() = Some(child);
        Ok(session)
    }

    /// A session over any pipe pair; `spawn` and the tests use it.
    pub fn from_io(
        stdout: impl AsyncRead + Send + Unpin + 'static,
        stdin: impl AsyncWrite + Send + Unpin + 'static,
        prompt_key: u64,
        mandate_hash: Option<String>,
        spontaneous: mpsc::UnboundedSender<LiveTurn>,
    ) -> Arc<Self> {
        let state = Arc::new(Mutex::new(State::default()));
        let reader_state = Arc::clone(&state);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut s = reader_state.lock().unwrap();
                for event in parse_line(&line) {
                    s.apply(event, &spontaneous);
                }
            }
            reader_state.lock().unwrap().end();
        });
        Arc::new(Self {
            stdin: tokio::sync::Mutex::new(Box::new(stdin)),
            state,
            child: Mutex::new(None),
            prompt_key,
            mandate_hash,
            started: Instant::now(),
        })
    }

    /// Send a message and wait for the turn it starts. Errors when the
    /// session ends first or the turn outlasts `timeout`; the caller should
    /// then [`Self::stop`] it.
    pub async fn turn(&self, text: &str, timeout: Duration) -> anyhow::Result<LiveTurn> {
        let (tx, rx) = oneshot::channel();
        {
            let mut s = self.state.lock().unwrap();
            if s.ended {
                anyhow::bail!("the session has ended");
            }
            s.waiting.push_back(tx);
        }
        let line = serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": text },
        })
        .to_string();
        {
            let mut stdin = self.stdin.lock().await;
            stdin.write_all(line.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await?;
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(turn)) => Ok(turn),
            Ok(Err(_)) => anyhow::bail!("the session ended during the turn"),
            Err(_) => anyhow::bail!("no answer within {}s", timeout.as_secs()),
        }
    }

    pub fn ended(&self) -> bool {
        self.state.lock().unwrap().ended
    }

    /// A turn is running or waiting, or a background command is.
    pub fn busy(&self) -> bool {
        let s = self.state.lock().unwrap();
        !s.ended && (!s.waiting.is_empty() || s.current.is_some() || !s.background.is_empty())
    }

    /// Background commands still running.
    pub fn background(&self) -> usize {
        self.state.lock().unwrap().background.len()
    }

    /// Since the session last did anything.
    pub fn idle_for(&self) -> Duration {
        let s = self.state.lock().unwrap();
        s.last_activity.unwrap_or(self.started).elapsed()
    }

    /// End the process and everything it started.
    pub fn stop(&self) {
        if let Some(child) = self.child.lock().unwrap().take() {
            #[cfg(unix)]
            if let Some(pid) = child.id() {
                // SAFETY: killpg takes plain integers and has no memory effects.
                unsafe {
                    libc::killpg(pid as libc::pid_t, libc::SIGKILL);
                }
            }
            drop(child);
        }
        self.state.lock().unwrap().end();
    }
}

impl Drop for LiveSession {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A stable key for a system prompt.
pub fn prompt_key(system_prompt: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    system_prompt.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{duplex, AsyncBufReadExt, AsyncWriteExt, BufReader};

    /// A fake CLI on the other end of the pipes: what it reads, and a way to
    /// print events.
    struct Fake {
        out: tokio::io::DuplexStream,
        input: tokio::io::Lines<BufReader<tokio::io::DuplexStream>>,
    }

    impl Fake {
        async fn say(&mut self, v: Value) {
            self.out
                .write_all(format!("{v}\n").as_bytes())
                .await
                .unwrap();
        }
        async fn read(&mut self) -> Value {
            let line = self.input.next_line().await.unwrap().unwrap();
            serde_json::from_str(&line).unwrap()
        }
        async fn echo(&mut self) {
            let msg = self.read().await;
            let mut echo = msg.clone();
            echo["isReplay"] = Value::Bool(true);
            self.say(echo).await;
        }
        async fn send(&mut self, room: &str) {
            self.say(serde_json::json!({"type": "assistant", "message": {"content": [
                {"type": "tool_use", "name": SEND_TOOL, "input": {"room_id": room, "text": "hi"}}
            ]}}))
            .await;
        }
        async fn result(&mut self, text: &str) {
            self.say(serde_json::json!({"type": "result", "subtype": "success", "result": text}))
                .await;
        }
    }

    fn session() -> (Arc<LiveSession>, Fake, mpsc::UnboundedReceiver<LiveTurn>) {
        let (ours_out, theirs_out) = duplex(1 << 16);
        let (ours_in, theirs_in) = duplex(1 << 16);
        let (tx, rx) = mpsc::unbounded_channel();
        let s = LiveSession::from_io(ours_out, ours_in, 1, None, tx);
        let fake = Fake {
            out: theirs_out,
            input: BufReader::new(theirs_in).lines(),
        };
        (s, fake, rx)
    }

    const T: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn a_turn_ends_with_its_result_and_knows_where_it_sent() {
        let (s, mut cli, _rx) = session();
        let turn = tokio::spawn({
            let s = Arc::clone(&s);
            async move { s.turn("hello", T).await }
        });
        let msg = cli.read().await;
        assert_eq!(msg["message"]["content"], "hello");
        let mut echo = msg.clone();
        echo["isReplay"] = Value::Bool(true);
        cli.say(echo).await;
        cli.send("general").await;
        cli.result("done").await;
        let turn = turn.await.unwrap().unwrap();
        assert!(!turn.spontaneous);
        assert!(turn.answered("general"));
        assert!(!turn.answered("other"));
        assert_eq!(turn.result["result"], "done");
    }

    #[tokio::test]
    async fn a_turn_the_cli_starts_itself_is_spontaneous() {
        let (s, mut cli, mut rx) = session();
        cli.say(serde_json::json!({"type": "system", "subtype": "task_started", "task_id": "b1"}))
            .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(s.background(), 1);
        assert!(s.busy());
        cli.say(serde_json::json!({"type": "system", "subtype": "task_notification", "task_id": "b1", "status": "completed"}))
            .await;
        cli.send("general").await;
        cli.result("FINISHED").await;
        let turn = tokio::time::timeout(T, rx.recv()).await.unwrap().unwrap();
        assert!(turn.spontaneous);
        assert!(turn.answered("general"));
        assert_eq!(s.background(), 0);
        assert!(!s.busy());
    }

    #[tokio::test]
    async fn our_message_after_a_spontaneous_turn_gets_its_own_result() {
        let (s, mut cli, mut rx) = session();
        // The CLI is in a turn of its own when our message is written.
        cli.say(serde_json::json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "checking"}]}}))
            .await;
        let ours = tokio::spawn({
            let s = Arc::clone(&s);
            async move { s.turn("second", T).await }
        });
        let msg = cli.read().await;
        cli.result("own turn").await;
        let mut echo = msg.clone();
        echo["isReplay"] = Value::Bool(true);
        cli.say(echo).await;
        cli.send("general").await;
        cli.result("ours").await;

        let own = tokio::time::timeout(T, rx.recv()).await.unwrap().unwrap();
        assert!(own.spontaneous);
        assert!(!own.answered("general"));
        let ours = ours.await.unwrap().unwrap();
        assert_eq!(ours.result["result"], "ours");
        assert!(ours.answered("general"));
    }

    #[tokio::test]
    async fn a_session_that_ends_fails_the_turn_waiting_on_it() {
        let (s, mut cli, _rx) = session();
        let turn = tokio::spawn({
            let s = Arc::clone(&s);
            async move { s.turn("hello", T).await }
        });
        cli.echo().await;
        drop(cli);
        assert!(turn.await.unwrap().is_err());
        assert!(s.ended());
        assert!(s.turn("again", T).await.is_err());
    }

    #[tokio::test]
    async fn a_turn_that_takes_too_long_times_out() {
        let (s, mut cli, _rx) = session();
        let turn = tokio::spawn({
            let s = Arc::clone(&s);
            async move { s.turn("hello", Duration::from_millis(100)).await }
        });
        cli.echo().await;
        let err = turn.await.unwrap().unwrap_err();
        assert!(err.to_string().contains("no answer"), "{err}");
    }

    #[test]
    fn lines_that_are_not_events_are_ignored() {
        assert_eq!(parse_line("not json"), vec![Event::Ignore]);
        assert_eq!(
            parse_line(r#"{"type":"system","subtype":"init"}"#),
            vec![Event::Ignore]
        );
        assert_eq!(
            parse_line(r#"{"type":"user","isReplay":true,"message":{"content":"x"}}"#),
            vec![Event::Echo]
        );
    }
}
