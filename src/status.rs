//! What the agent is doing right now, posted to the engine so people can see
//! it next to the agent's name (`POST /agents/{me}/status`).
//!
//! Best effort, like typing: reporting never fails or delays a turn. Updates
//! are queued and posted in order by one background task, which also
//! re-posts the current status every minute so the engine's 120 s expiry
//! never lapses while the agent is up.

use crate::nubi::NubiClient;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// How often the current status is re-posted, and how often a failing post
/// may be logged.
pub const STATUS_REFRESH: Duration = Duration::from_secs(60);

/// The engine takes at most this many characters of text.
const MAX_TEXT: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Working,
    Idle,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Working => "working",
            State::Idle => "idle",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub state: State,
    pub room_id: Option<String>,
    pub text: String,
}

impl Status {
    pub fn new(state: State, room_id: Option<&str>, text: &str) -> Self {
        Self {
            state,
            room_id: room_id.map(String::from),
            text: clip(text),
        }
    }
}

/// One line of at most [`MAX_TEXT`] characters, cut on a char boundary.
fn clip(text: &str) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= MAX_TEXT {
        return one_line;
    }
    let mut cut: String = one_line.chars().take(MAX_TEXT - 1).collect();
    cut.push('…');
    cut
}

#[derive(Default)]
struct Board {
    /// The last status queued: what the engine shows (or soon will).
    current: Option<Status>,
    /// Turns running now, oldest first, one per room. While any runs, the
    /// newest is what the agent is doing; other updates are posted (so they
    /// reach the log) and then the turn's status is posted again.
    turns: Vec<Status>,
}

pub struct StatusReporter {
    board: Mutex<Board>,
    tx: mpsc::UnboundedSender<Status>,
    rx: Mutex<Option<mpsc::UnboundedReceiver<Status>>>,
}

impl Default for StatusReporter {
    fn default() -> Self {
        Self::new()
    }
}

impl StatusReporter {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            board: Mutex::new(Board::default()),
            tx,
            rx: Mutex::new(Some(rx)),
        }
    }

    /// Start posting through `nubi`. Needs a tokio runtime; later calls do
    /// nothing. Updates made before it are queued, not lost.
    pub fn start(self: &Arc<Self>, nubi: NubiClient) {
        if let Some(rx) = self.rx.lock().unwrap().take() {
            tokio::spawn(post_loop(Arc::downgrade(self), nubi, rx));
        }
    }

    /// A turn started in `room_id`.
    pub fn begin(&self, room_id: &str, text: &str) {
        let status = Status::new(State::Working, Some(room_id), text);
        let mut board = self.board.lock().unwrap();
        board
            .turns
            .retain(|t| t.room_id.as_deref() != Some(room_id));
        board.turns.push(status.clone());
        self.queue(&mut board, status);
    }

    /// The turn in `room_id` ended, with this outcome.
    pub fn end(&self, room_id: &str, state: State, text: &str) {
        let mut board = self.board.lock().unwrap();
        board
            .turns
            .retain(|t| t.room_id.as_deref() != Some(room_id));
        self.event_locked(&mut board, Status::new(state, Some(room_id), text));
    }

    /// Something happened outside a turn (a session started or closed,
    /// background work). It becomes the status unless a turn is running.
    pub fn event(&self, status: Status) {
        let mut board = self.board.lock().unwrap();
        self.event_locked(&mut board, status);
    }

    pub fn current(&self) -> Option<Status> {
        self.board.lock().unwrap().current.clone()
    }

    fn event_locked(&self, board: &mut Board, status: Status) {
        self.queue(board, status);
        if let Some(turn) = board.turns.last().cloned() {
            self.queue(board, turn);
        }
    }

    /// Queue a post unless it repeats the last one (the refresh covers that).
    /// Under the board lock, so posts go out in the order they were made.
    fn queue(&self, board: &mut Board, status: Status) {
        if board.current.as_ref() == Some(&status) {
            return;
        }
        board.current = Some(status.clone());
        let _ = self.tx.send(status);
    }
}

async fn post_loop(
    reporter: Weak<StatusReporter>,
    nubi: NubiClient,
    mut rx: mpsc::UnboundedReceiver<Status>,
) {
    let mut ticker =
        tokio::time::interval_at(tokio::time::Instant::now() + STATUS_REFRESH, STATUS_REFRESH);
    let mut logged: Option<Instant> = None;
    loop {
        let status = tokio::select! {
            next = rx.recv() => match next {
                Some(s) => s,
                // The reporter (and its sender) is gone.
                None => return,
            },
            _ = ticker.tick() => {
                let Some(reporter) = reporter.upgrade() else { return };
                let Some(s) = reporter.current() else { continue };
                s
            }
        };
        let posted = nubi
            .set_status(
                status.state.as_str(),
                status.room_id.as_deref(),
                &status.text,
            )
            .await;
        if let Err(e) = posted {
            if logged.is_none_or(|t| t.elapsed() >= STATUS_REFRESH) {
                eprintln!("[nubi-agent] Failed to post status: {}", e);
                logged = Some(Instant::now());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(rx: &mut mpsc::UnboundedReceiver<Status>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(s) = rx.try_recv() {
            out.push(format!(
                "{} {} {}",
                s.state.as_str(),
                s.room_id.unwrap_or_default(),
                s.text
            ));
        }
        out
    }

    #[test]
    fn long_text_is_cut_to_200_chars_on_a_char_boundary() {
        let s = Status::new(State::Idle, None, &"é".repeat(500));
        assert_eq!(s.text.chars().count(), 200);
        assert!(s.text.ends_with('…'));
        assert_eq!(Status::new(State::Idle, None, " a\n b ").text, "a b");
    }

    #[test]
    fn events_during_a_turn_are_posted_then_the_turn_again() {
        let r = StatusReporter::new();
        let mut rx = r.rx.lock().unwrap().take().unwrap();
        r.begin("general", "answering bob");
        r.event(Status::new(
            State::Idle,
            Some("general"),
            "Claude session started",
        ));
        r.end("general", State::Idle, "replied");
        // Same again: nothing new to post.
        r.event(Status::new(State::Idle, Some("general"), "replied"));
        assert_eq!(
            drain(&mut rx),
            [
                "working general answering bob",
                "idle general Claude session started",
                "working general answering bob",
                "idle general replied",
            ]
        );
        assert_eq!(r.current().unwrap().text, "replied");
    }

    #[test]
    fn a_turn_ending_while_another_runs_hands_the_status_back() {
        let r = StatusReporter::new();
        let mut rx = r.rx.lock().unwrap().take().unwrap();
        r.begin("a", "answering bob");
        r.begin("b", "answering eve");
        r.end("b", State::Idle, "replied");
        assert_eq!(
            drain(&mut rx),
            [
                "working a answering bob",
                "working b answering eve",
                "idle b replied",
                "working a answering bob",
            ]
        );
    }
}
