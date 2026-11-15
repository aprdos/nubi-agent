# One Claude session per agent: design

Written 2026-11-15 against `origin/main` at `e3e6c1c` (after #47). Status:
proposal, nothing built.

## The question

An agent in live mode runs one `claude` process per room it is talking in
(`Agent.live`, keyed by room id, see `live_session` in `src/agent.rs`). Apple
answering in `#general`, a PR channel and a thread is three Claude processes,
three conversations, three contexts. Seen on managed prod: four processes in
one sandbox, about 700 MB together.

The question from the user: should an agent be **one** Claude that hears
every room, possibly spawning subagents for heavy work, instead of one per
room?

## What per-room costs today

1. **Memory.** Each process is a Node runtime plus its own context and MCP
   connection, roughly 150–250 MB. `live_max` (3) caps it by stopping the
   one idle longest; `live_idle_secs` (900 since #47) closes quiet ones.
2. **Knowledge split.** A turn in room A knows room B only through
   `elsewhere()`: at most 8 lines of "running now in B" and B's journal
   (24 h for its thread/parent family, 1 h for others). It sees that
   something happened, not the reasoning or the files read.
3. **Duplicated work.** Two rooms asking for the same thing can both start
   it; `elsewhere()` makes that less likely, not impossible.
4. **Background work is tied to a room.** A `run_in_background` command
   wakes the session that started it, so its follow-up lands in that room's
   context only.

## What per-room buys today

1. **Clean contexts.** A room's conversation is not diluted by others; a
   long debugging thread does not push a quick question in `#general`
   towards compaction.
2. **No cross-room leaks by construction.** A DM's content is not in the
   context that answers a public channel (except the journal lines
   `elsewhere()` deliberately adds).
3. **Isolation of failures.** A session that crashes, hangs or gets a bad
   resume affects one room; `drop_live` restarts just that one.
4. **Simple attribution.** One room per session makes "did this turn
   answer its room" (`LiveTurn::answered`, the re-ask) easy.

Parallelism is **not** something per-room buys in practice:
`max_parallel_turns` defaults to 1 and managed does not raise it, so turns
already run one at a time across all rooms.

## Options

### A. Keep per-room, share more (status quo+)

Improve `elsewhere()`: longer summaries per turn (what was decided, which
PR/branch), a shared "work in progress" file in the workspace each session
reads and updates, a hint in the prompt to check it before starting work.

- Cheap, no change to how sessions run.
- Memory stays N processes.
- Knowledge stays second-hand.

### B. One session per agent

Key `Agent.live` by a fixed key instead of the room id. Every message is
written to the same process with its room in the header; the agent
answers with `nubi_send(room_id, …)` as today. The prompt already opens
with the room (`[Group room — room_id: …]`, `[DM with …]`, `[Thread — …]`,
see `build_prompt` in `src/harness/mod.rs`).

What changes:

- **Turn bookkeeping.** `LiveTurn.sent_to` and `answered(room)` already
  work per room. The re-ask has to be decided per *trigger room* of the
  turn, which may now be several rooms steered into one turn.
- **Steering.** A message for room B while a turn for room A runs is
  steered into that turn (today only same-room messages are). The model
  then handles both in one turn; `sent_to` tells us which got answers.
- **Session record.** `sessions.json` holds one session id for the agent
  instead of one per room; the per-room journal and `last_index` stay.
- **Prompt key.** The system prompt is per agent (mandate, skills), so one
  key; nothing per room to restart for.
- **Context.** All rooms share one context. Compaction comes sooner, and
  after it the model knows rooms only from the summary — the same problem
  as today's `elsewhere()`, moved inside the session.
- **Leaks.** Content from a DM or private room is in the context that
  answers a public room. Needs a rule in the system prompt and, better, a
  check: rooms with different member sets must not share a session (see
  B′).
- **Failure.** One crash or hang stops the agent in every room until the
  restart.

- Memory: one process.
- Knowledge: first-hand across rooms.

### B′. One session per audience

As B, but keyed by *who can read the room*: rooms whose members are the
same set (a channel and its threads; the PR channel and the GitHub room of
the same team) share a session, a DM gets its own. Same mechanics as B,
removes the leak problem, keeps more than one process only where privacy
needs it. Needs the room's member list (the engine has it; the agent would
fetch and cache it).

### C. Orchestrator with subagents

B plus explicit delegation: the one session stays light — it reads,
decides, answers — and hands heavy work (a build, a review, a long
investigation) to Claude Code subagents (`Task`) or background commands.
The subagent's result comes back into the main session, which posts it to
the right room.

- Keeps the main context small: the heavy reading lives in the subagent's
  context and only the summary returns.
- This is a prompt/skill change on top of B, not a different runtime: the
  CLI already supports subagents and background tasks in a live session.
- Subagents are separate model calls; in one process they don't cost a
  process each, but they do cost tokens.

## Recommendation

1. **Now:** A's cheapest part — make the journal line per turn say what was
   done (PR, branch, decision), not just "replied". Small, no design
   risk, helps every option below.
2. **Next:** B′ behind a flag, `NUBI_LIVE_SCOPE=room|audience` (default
   `room`). Try it on one managed agent (apple) for a few days.
3. **Then:** C as a skill ("delegate heavy work to a subagent, answer from
   its summary") once B′ holds up.

Go straight to B only if member-set checks turn out to be too costly; then
put the "never repeat a private room's content elsewhere" rule in the
system prompt and accept the risk explicitly.

## How we'd know it works

Measured on the trial agent against the same agent before:

- processes and RSS per sandbox (target: 1–2 instead of 3–4);
- duplicate work: two rooms starting the same PR/review/build (log grep
  for the same branch or PR in two rooms' turns within an hour);
- answers that miss context from another room (spot-check transcripts);
- compactions per day and turn latency (one queue for every room);
- no private-room content in public rooms (spot-check).

## Open questions

- **Where does the member set come from?** `GET /rooms/{id}` members, cached
  and refreshed on membership events? Threads inherit their parent's.
- **Steering across rooms:** steer a message for another room into a
  running turn, or queue it for the next turn? Steering is faster but
  makes a turn answer several rooms; queuing keeps turns one-room.
- **Re-ask with several trigger rooms:** nudge once per unanswered room, or
  once naming all of them?
- **Restart cost:** one session failing takes every room with it. Is
  `drop_live` + resume enough, or do we want a per-room fallback?
- **Webhook rooms** (`github-*`): they're read-only for the agent and their
  events come from `system`. Same audience as the team channel?
