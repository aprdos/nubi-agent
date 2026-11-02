# nubi-agent audit: how to make the agent smarter

Date: 2026-11-02. Scope: the `nubi-agent` repository at commit `b55c909`, plus the
live state it leaves on this machine (`~/.nubi-agents/*`, `logs/*.log`) and the
`nubi-mcp` tool definitions it hands to the model. No code was changed.

Model-usage judgments below were checked against the `claude-api` skill
(current model table, thinking/effort rules, prompt-audit patterns) and the
installed Claude Code CLI (`2.1.137`), not from memory.

## Status (2026-11-02, branch `feat/smarter-agent`)

All 13 findings below were implemented after the audit, with one correction
to finding 1: the local gateway's upstream rejects every model newer than
Opus 4.5 (checked with real requests: Opus 4.6, 4.7, 4.8, 5 and 5.5 all
return `INVALID_MODEL_ID`; Opus 4.5, Sonnet 4.6, Sonnet 4.5 and Haiku 4.5
work). The default therefore stays `claude-opus-4-5`, dashed: the gateway accepts
dotted ids too, but Claude Code hangs on `claude-opus-4.5` (verified; the new
turn timeout kills it). `claude-sonnet-4-6` is the fallback model
and `--effort high` as the new lever. Revisit when the gateway serves a newer
Opus. Claude Code's auto-compaction is set to trigger at 60% of the context
window via `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE` (`--compact-at-pct`).

## TL;DR

nubi-agent is a solid *transport*: SSE in, one `claude -p` run per message,
text out. The mandate/skills/hash machinery is careful and well tested. What
limits how smart the agent *is* lies almost entirely in the thin layer between
transport and model:

1. Every message is a brand-new process with no session, so the agent forgets
   its own reasoning, tool results and files between consecutive messages in
   the same room. Its only memory is a hand-maintained `CLAUDE.md`.
2. The default model is two Opus generations old and no effort level is set.
3. The harness reads the CLI's output as plain text, so it cannot tell "the
   agent answered via `nubi_send`" from "the agent crashed", has no timeout,
   never retries, and never tells the room when a turn failed. 543 of 2,572
   logged runs ended as "Empty response, skipping".
4. The prompt the model sees is 30 flat `sender: text` lines with no message
   indexes, timestamps, mention metadata or thread anchor, so the tools that
   need an index (`nubi_create_thread`, `nubi_poll before_index`) are hard to
   use correctly, and threads are presented as anonymous group rooms.
5. The system prompt is long, bullet-heavy and prohibition-heavy, names a
   stale tool list, and replaces Claude Code's own system prompt rather than
   appending to it. Several post-processing heuristics (skip "yes"/"no",
   truncate at "Human:") silently eat legitimate answers.

The top five recommendations, in order, are in the final section. Each
finding below says what the code does, why it limits the agent, and the
concrete change.

## How it runs today

- **Entry**: `src/main.rs` parses flags/env, connects to the engine
  (`/v1/me`), builds `AgentConfig`, calls `Agent::connect` then `run()`.
- **Loop**: `src/agent.rs` subscribes to the space SSE stream, buffers every
  message per room (`RoomBuffer`, 100 entries), and when a message mentions
  the agent (or is a DM) and `expect_reply` includes it, queues a `Trigger`.
  One worker per room; at most 4 concurrent harness processes; 500 ms
  coalescing; 5 s per-room cooldown.
- **Turn**: `Shared::reply` (`agent.rs:909`) checks the mandate, primes the
  buffer from server history once per room, builds a prompt from the last 30
  messages, spawns the harness with a typing indicator, post-filters the text
  and POSTs it with `expect_reply: []`.
- **Harness**: `src/harness/mod.rs` `CliRuntime` renders the system prompt
  (identity → mandate → built-in instructions → `--system-prompt` → ad-hoc
  skills) and spawns one of seven CLIs. For Claude: `claude -p <prompt>
  --model <m> --system-prompt <sp> --mcp-config <file>
  --allowedTools=mcp__nubi` in `~/.nubi-agents/<agent>/` (`harness/claude.rs:46`).
- **Model**: default `claude-opus-4.5` through a local gateway at
  `http://localhost:5000` (`main.rs:108-117`). No effort, no output format, no
  timeout, no max budget.
- **Memory**: the seeded `CLAUDE.md` in the workspace (`agent.rs:386`), which
  the model is told to edit itself. Skills come from `~/.nubi/skills`,
  `~/.nubi-agents/<agent>/skills`, skill rooms, and the engine-resolved
  mandate, all inlined into the system prompt.
- **Tests**: good unit coverage of mention parsing, buffer, mandate hashing
  and refresh races against a mock engine. Nothing exercises prompt quality,
  the real CLI, or end-to-end behaviour.

## Ranked findings

Impact = expected effect on answer/action quality. Effort = implementation
size. Cheap, high-impact items first.

| # | Finding | Impact | Effort |
|---|---------|--------|--------|
| 1 | Model is `claude-opus-4.5`, no effort setting | High | Trivial |
| 2 | Output parsed as text: errors, tool-only turns and empty turns are indistinguishable | High | Small |
| 3 | No timeout, no retry, no fallback model, silent failure to the room | High | Small |
| 4 | Fresh process per message, no session continuity | High | Medium |
| 5 | Prompt context is lossy: no indexes, times, mentions, thread anchor | High | Small |
| 6 | Coalescing drops earlier triggers from the "new message" slot | Medium | Small |
| 7 | Post-filters swallow real answers (`yes`, `no`, `done`, "Human:") | Medium | Trivial |
| 8 | System prompt: bloat, prohibitions, stale tool list, replaces Claude Code's prompt | Medium | Small |
| 9 | Plain-text replies can never mention or ask; structured output would fix it | Medium | Small |
| 10 | Skills inlined in full; no progressive disclosure; `trigger` ignored | Medium | Medium |
| 11 | Memory is one file per agent, duplicated across agents, no record of actions | Medium | Medium |
| 12 | No observability or evals: no duration, cost, tokens, outcome class | Medium | Small→Medium |
| 13 | Dead/misleading config: heartbeat, README defaults, repo `CLAUDE.md` | Low | Trivial |

## Findings in detail

### 1. Model and effort (High / Trivial)

**Today.** `main.rs:114` defaults the Claude harness to `claude-opus-4.5`;
`harness/claude.rs:52-54` passes it as `--model`. Nothing passes `--effort`.
The user's shell also aliases `claude` to `--model claude-opus-4.5`, so even
manual smoke tests run the old model.

**Why it limits the agent.** Per the current model table, Opus 4.5 is two
Opus generations behind `claude-opus-5-5` (1M context, adaptive thinking
always on, much stronger long-horizon tool use). On Opus 5.5 the effort
default is `medium`; Claude Code's own default is `xhigh` for agentic work.
Effort is the single lever that most changes tool-call thoroughness and
verification, and the agent is doing exactly that kind of work (PR review,
multi-step coordination, `gh` lookups).

**Change.**
- Default `--model` to `claude-opus-5-5` (whatever dotted/dashed form the
  gateway on `:5000` maps; the comment at `main.rs:64` says it wants dotted
  IDs, so confirm `claude-opus-5.5` resolves there). Keep the override.
- Add `--effort <low|medium|high|xhigh|max>` (env `NUBI_LLM_EFFORT`), default
  `high`, forwarded as `--effort` for the Claude harness (`claude --help`
  lists it). Consider `xhigh` for reviewer agents (pine, palm) and `medium`
  for chatty coordination agents; make it per-agent via the existing flag.
- Add `--fallback-model` (Claude Code supports it with `-p`) so an overloaded
  primary degrades to Sonnet 5.5 instead of failing the turn.

### 2. Output is read as plain text (High / Small)

**Today.** `harness/claude.rs:46-71` runs with the default `--output-format
text`. `CliRuntime::run` (`harness/mod.rs:516-592`) takes stdout as the
answer. `Shared::reply` (`agent.rs:937-940`) treats an empty string as "the
agent chose to say nothing" and skips. The logs show 543 of 2,572 runs
(21%; 46% for `pine`) ended that way, and the three logged hard failures
read `Claude failed: ` with an empty detail (`mod.rs:573-581` prints stderr
or stdout, both were empty).

**Why it limits the agent.** An empty stdout has at least four different
causes: the model replied via `nubi_send` (intended), the CLI hit an API
error, the run exhausted turns, or the model produced only tool calls and
stopped. The harness cannot tell them apart, so it cannot retry the right
ones, cannot report failures, and cannot measure anything. The sender in the
room sees nothing in every case.

**Change.** Run with `--output-format json`. One run on this machine shows
the result object carries `is_error`, `api_error_status`, `result`,
`num_turns`, `duration_ms`, `total_cost_usd`, `usage` (including cache
reads), `permission_denials`, `terminal_reason` and `session_id`. Parse it
in `Claude::parse_output`, return a richer `RunOutput` (text, is_error,
error_detail, num_turns, cost, session_id), and branch in `reply`:
- `is_error` → finding 3 (retry / report).
- empty text and `num_turns > 1` → tool-only turn; log as "answered via
  tools", not "empty".
- empty text and `num_turns <= 1` → the model genuinely said nothing; treat
  as a failure and retry once with a nudge ("you produced no reply; either
  answer in text or send via nubi_send").
- `permission_denials` non-empty → a tool the model wanted was blocked;
  surface it in the log and consider widening `--allowedTools`.

`--output-format json` also fixes the "Claude failed: " blank: errors land in
`result` even when stderr is empty.

### 3. No timeout, retry, fallback or failure feedback (High / Small)

**Today.** `CliRuntime::run` awaits the child with no deadline
(`mod.rs:537-565`). A hung CLI (gateway stall, MCP server not answering,
`gh` waiting on a prompt) holds the room's worker and one of four global
permits forever; the typing indicator keeps refreshing every 5 s. On any
error, `serve_room` (`agent.rs:903-905`) logs and moves on; nothing is posted.
`NubiClient::post` retries 5xx three times, but the LLM run itself never
retries. The gateway default at `main.rs:112` means a down gateway fails
every run with no visible cause.

**Why it limits the agent.** The code's own comment at `types.rs:111` calls a
missing reply a "silent deadlock" and fails open to avoid it, but a failed
run produces exactly that deadlock. Teammates wait, then re-ping ("@apple
ada progress?", "@manager Lu masih handle ini gak" both follow logged
failures). Each re-ping is another cold run with less context.

**Change.**
- Wrap the run in `tokio::time::timeout` (flag `--turn-timeout-secs`,
  default ~900) and kill the child on expiry; log `terminal_reason`.
- Retry once on `is_error` with transient causes (`api_error_status` 429/5xx,
  connection errors, timeout) after a short backoff; pass `--fallback-model`
  on the retry.
- On final failure, post a one-line plain message to the room ("couldn't
  finish that one: <short cause>, try again or ping me") so the sender is
  unblocked. Keep it `expect_reply: []`.
- Hold the typing indicator only while the child is alive (already true once
  a timeout exists).

### 4. No session continuity between messages (High / Medium)

**Today.** Each message is `claude -p` with a fresh session (`claude.rs:48`).
The only carry-over is the workspace directory and `CLAUDE.md`. The system
prompt tells the model this explicitly (`mod.rs:277-288`). Context is 30
lines of chat text from `RoomBuffer`; the model's own previous tool results,
files it read, `gh` output, and reasoning are gone.

**Why it limits the agent.** Most room conversations are multi-turn with the
same agent: "review PR #50" → "ci masih gagal" → "udah kah?". On the third
message the agent has to re-discover the PR, re-run `gh`, and re-read the
diff, or guess. The logs show follow-ups like "ada progress?" answered with
empty output. Claude Code already solves this: `--resume <session-id>` /
`--session-id <uuid>` continue a saved conversation, with the CLI's own
compaction handling length.

**Change.** Keep one Claude Code session per room (or per thread), stored in
the workspace (`.nubi/sessions.json`: room_id → session_id, last used, turn
count). On a trigger: if a session exists and is younger than N hours, run
`claude -p --resume <id> ...`; else start fresh with `--session-id <new
uuid>` and record it. Read `session_id` back from the JSON result (finding
2). Fall back to a fresh session if resume fails. Because the prompt prefix
(system prompt + conversation) is then stable across turns, prompt caching
starts paying off as well. Keep the room-history block in the prompt but
shrink it to "messages since your last turn" when resuming, so the model is
not shown the same 30 lines twice. Codex (`codex exec resume`) and Gemini
have equivalents; implement for Claude first.

This also softens the memory problem (finding 11): the model no longer has
to write everything it learned into `CLAUDE.md` to survive to the next
message.

### 5. The prompt context is lossy (High / Small)

**Today.** `CliRuntime::build_prompt` (`mod.rs:483-514`) renders
`- sender: text` per message and `[New message from X]`. `BufferedMessage`
(`buffer.rs:12-17`) carries `global_index` and `timestamp_ms` but neither is
printed. Mentions and `expect_reply` metadata are not kept in the buffer at
all (`BufferedMessage::from_entry` drops `payload.metadata` and `mentions`).
Thread rooms (`thread.<room>.<index>`) are labelled `[Group room]` with no
parent room or anchor message.

**Why it limits the agent.**
- `nubi_create_thread` needs `message_index`; `nubi_poll` needs
  `before_index`. The model never sees an index, so it must poll first or
  guess. The agents' own memory files show the workaround they learned
  ("Link PR biasanya ada di anchor msg thread — cek dulu sebelum nanya").
- Without timestamps the model cannot tell a message from two minutes ago
  from one from yesterday, which matters for "is this still true" judgments
  the system prompt spends 20 lines on.
- Without mention/expect_reply metadata the model cannot see which earlier
  messages were addressed to it and left unanswered.
- In a thread, the anchor message usually *is* the task (the PR link). The
  agent starts without it.

**Change.** Keep `mentions` and `expect_reply` in `BufferedMessage`. Render
each line as `[#1234 12:03 alice → @pine @palm] text` (index, relative or
HH:MM time, mention targets). State the trigger's index explicitly:
`[New message #1240 from alice, expects reply from: pine]`. For thread rooms,
parse the parent and anchor index, fetch the anchor via `history`/poll once,
and prepend `[Thread in <room> on message #540: <anchor text>]`. Size
context by tokens rather than count (e.g. up to ~6k tokens, newest first),
and make `--context-size` the cap rather than the exact count.

### 6. Coalescing keeps only the newest trigger (Medium / Small)

**Today.** `Dispatch.pending` is `HashMap<room, Trigger>`; `enqueue`
(`agent.rs:868-879`) overwrites, and the prompt names only that one message
as "new". Earlier mentions during the 500 ms window, or during a long run,
survive only as anonymous context lines.

**Why it limits the agent.** If alice asks one thing and bob another while a
run is in flight, bob's message becomes "the" message and alice's is just
history; the model often answers only the one framed as new. Dropped
questions look like the agent ignoring people.

**Change.** Keep a `Vec<Trigger>` per room and render all pending triggers as
`[Messages addressed to you since your last reply]` with indexes, so the
model answers each (or says it is handling them in order).

### 7. Post-filters swallow legitimate answers (Medium / Trivial)

**Today.** `agent.rs:947-971` drops any reply ≤30 chars equal to `ok`,
`done`, `sent`, `yes`, `no`, `empty`, `understood`, `got it`, `will do`,
`on it`. `agent.rs:974-989` drops replies starting with `Human:`/
`Assistant:` and truncates at a mid-text `Human:`.

**Why it limits the agent.** "@manager can be merged?" → "yes" or "no" is a
correct, complete answer and is discarded. "done" after a requested restart
is the status the system prompt tells the model to give ("Give status like a
person: done, blocked on X"). The `Human:` heuristics date from completion-
style models; with a system prompt that already forbids the prefix and a
model that does not produce it, they are fossils that can only lose text
(a code review quoting a transcript with "Human:" would be cut).

**Change.** Delete the skip-word list and the `Human:` truncation. The real
problem they papered over, "the model echoes a confirmation after using
`nubi_send`", is solved by finding 2 (detect tool-only turns) and finding 9
(structured output with an explicit `post: false`).

### 8. System prompt quality (Medium / Small)

**Today.** `CliRuntime::default_instructions` (`mod.rs:257-358`) is ~95
lines: a hard-coded tool list (`mod.rs:260`), five `Rules`, a `CRITICAL:` block,
`NEVER ... Not once`, and roughly 40 bullets of behavioural guidance,
followed by the user's `--system-prompt`, then skills. `harness/claude.rs:55`
passes it as `--system-prompt`, which *replaces* Claude Code's default
system prompt rather than appending.

**Why it limits the agent.** Checked against the prompt-audit patterns in the
`claude-api` skill:
- *Tool names in the system prompt* (Group 3): the list at `mod.rs:260`
  omits `nubi_create_thread` and `nubi_list_skills`, which `nubi-mcp` ships
  and which the PR-review process in every agent's memory depends on. The
  MCP server already provides good descriptions (`nubi_send`'s is
  excellent); the prose list duplicates them and goes stale.
- *Pressure language and prohibition clusters* (1a, 1c, 1e): `CRITICAL`,
  `NEVER`, "Not once", numbered `Rules`. Current models over-trigger on
  these; the voice section in particular is a wall of "don't"s, and bullet
  walls flatten priority and bleed into output format.
- *Replacing the default system prompt*: Claude Code's own prompt carries
  its tool-use guidance for Bash/Edit/Read, git conventions and safety
  rules. Dropping it means the agent runs the coding tools with none of
  that. `--append-system-prompt` keeps it; `--bare` plus
  `--append-system-prompt` is the lean variant if startup time matters.
- Rule 2, "Never describe yourself as an AI, model, bot, or assistant", asks
  the model to deny what it is if someone asks directly. That is an honesty
  problem as well as a quality one; "don't volunteer it, don't be
  assistant-ish" achieves the persona goal without the denial.
- The memory section says "Nothing carries over" which becomes false once
  finding 4 lands; keep the prompt in sync.

**Change.** Rewrite as short prose paragraphs carrying the *why*: who you
are, how mentions/expect_reply work (keep, it is the one genuinely non-
obvious mechanic), where plain text goes vs `nubi_send`, check live state
before asserting it, voice in two or three sentences. Remove the tool list
and let MCP descriptions do that job. Switch to `--append-system-prompt`.
Drop rule 2's denial. Measure before/after on the eval in finding 12.

### 9. Plain-text replies cannot mention or ask (Medium / Small)

**Today.** `NubiClient::post` (`nubi.rs:151-165`) always sends `mentions: []`
and `expect_reply: []`. The system prompt spends a section (`mod.rs:304-313`)
teaching the model that plain text "cannot ask a question" and that it must
switch to `nubi_send` for that, and a `CRITICAL` block warning it not to
also echo text after `nubi_send`.

**Why it limits the agent.** This is a harness limitation turned into a
model rule. It produces the double-post problem, the "thread went quiet"
failure the prompt describes, and the skip-word hack. The model has to hold
a routing decision in its head on every reply.

**Change.** Use Claude Code's `--json-schema` structured output and ask for
`{ "text": string, "mentions": string[], "expect_reply": string[],
"post": boolean }`. nubi-agent posts `text` with the given mentions and
expect_reply (the engine already dedups by message_id). `post: false` is the
explicit "I already handled this via tools" signal, replacing the skip-word
heuristic. The `expect_reply: []` loop-brake stays the default when the
model omits it. The `nubi_send` tool remains for other rooms.

### 10. Skills are inlined in full (Medium / Medium)

**Today.** `SkillLoader::build_prompt_section` (`skills.rs`) and
`build_mandate_section` (`mandate.rs`) paste every enabled skill's full body
into the system prompt on every run. The `trigger` frontmatter is rendered as
`**Trigger:**` text; the shared skill on disk uses `triggers:` (plural),
which the parser does not read, so it is silently dropped. Agents have
started keeping their own skills in `~/.nubi-agents/<agent>/.claude/skills/`
(palm, pine), which Claude Code loads on demand, in parallel to this system.

**Why it limits the agent.** Every token of skill text is paid on every
message whether relevant or not, and it competes with the room context for
attention. Progressive disclosure (description in context, body on demand)
is the pattern Claude Code and the API skills feature both use.

**Change.** For mandated (required) skills, keep inlining: they must always
apply. For suggested and ad-hoc skills, write each to the workspace as
`.claude/skills/<name>/SKILL.md` with its frontmatter (name, description,
trigger) and inject only a one-line index into the system prompt. Parse both
`trigger` and `triggers`. Re-write the files when the mandate changes
(there is already a hook for that in `inject_mandate`).

### 11. Memory: one hand-written file per agent (Medium / Medium)

**Today.** `CLAUDE.md` in each workspace, seeded at `agent.rs:386-403`,
maintained by the model. It works: the six files on disk hold real standing
instructions. But the same "PR review process" is copied, slightly
differently, into all six; nothing records what the agent *did* (which PRs
it reviewed, what it said, when); and there is no per-room memory.

**Why it limits the agent.** Team-wide rules drift between agents. "Ada
progress?" needs a record of the agent's own last actions, which no file
holds, so the agent either re-derives or answers nothing. Memory writes also
compete with answering in a single turn.

**Change.**
- A shared team memory file (`~/.nubi/TEAM.md`) passed via `--add-dir` or
  included in the system prompt, owned by humans or a designated agent, for
  cross-agent rules; per-agent `CLAUDE.md` for the agent's own notes.
- A per-room journal nubi-agent writes itself after every turn
  (`~/.nubi-agents/<agent>/rooms/<room>.md`: timestamp, trigger index,
  reply or "replied via tools", session id). Include the last few entries in
  the prompt for that room. With finding 4 in place this is a safety net
  rather than the primary memory.

### 12. No observability, no evals (Medium / Small → Medium)

**Today.** Logs record "Running claude..." and "Responding (N chars)". No
duration, model, cost, token counts, cache hits, turn count or failure
class. CI runs fmt/clippy/unit tests. `mock_engine.rs` and `FakeRuntime`
exist but nothing checks prompt quality or outcomes.

**Why it limits the agent.** Every change in this document needs a before/
after. Today the only signal is reading room transcripts.

**Change.** With finding 2's JSON result, log one structured line per turn
(room, trigger index, outcome class, num_turns, duration_ms, cost,
cache_read tokens, session reused or not). Then build a small eval: 30–50
real triggers with their room context pulled from the engine, graded by an
LLM judge on (a) did it answer what was asked, (b) if it asked something,
did it set `expect_reply`, (c) did it check live state before asserting it,
(d) no double post. The `claude-api` skill's `build-eval` flow is the
template. Run it on model, effort and prompt changes.

### 13. Dead and misleading configuration (Low / Trivial)

- `--heartbeat-secs` / `heartbeat_interval_secs` is parsed and stored but
  never read (`agent.rs:57`, `main.rs:127`); the README still documents
  heartbeat messages. Typing refresh is hard-coded to 5 s
  (`agent.rs:1007`). Remove the flag or wire it to the typing interval.
- README says `--context-size` defaults to 10 and `--url` to
  `localhost:9099`; code says 30 and `https://chat.nubi.chat`
  (`main.rs:13,38`). Harness table says Claude's token goes via a 0600
  temp file, but `claude.rs:34` writes it into the MCP config's
  `Authorization` header (that file is 0600, so the claim is roughly true
  but the mechanism differs).
- `AgentRuntime::inject_context` and `Agent::skills`/`room_skill_loader`
  are dead (`#[allow(dead_code)]`); the cache-invalidation-on-SSE the comment
  promises does not exist. Either implement room-skill refresh on a
  `skill_update` event or drop the field.
- The repository's `CLAUDE.md` ("AWS CLI is installed", "SSH config
  accessible") is advice to Claude Code working *on* this repo, not to the
  agents; fine, but easy to mistake for runtime instructions.
- `PLAN.md` is the original proof-of-concept plan with every task still
  unchecked; archive it or replace with a current roadmap (this document
  could seed it).

## What to keep

- The mandate pipeline (fetch, ETag, hash recomputation, refuse-on-mismatch,
  stamped replies) is careful and well tested. Do not touch it while doing
  the above.
- Fail-open `expects_reply`, the `expect_reply: []` loop-brake on posts,
  per-room workers with a global permit cap, offset commits, history
  priming on first reply, and the workspace-per-agent isolation are all the
  right calls.
- The harness abstraction (`Harness` trait, one adapter per CLI) makes
  findings 1–4 and 9 mostly Claude-adapter changes plus a richer `RunOutput`.
- `nubi-mcp`'s `nubi_send` description is a model of what a tool
  description should be; bring `nubi_poll`/`nubi_search` up to that level
  (when to use which) and the system prompt can stop explaining them.

## Suggested order

1. Findings 1 + 2 + 3 together (model/effort, JSON output, timeout/retry/
   failure message): one PR, mostly `harness/claude.rs`, `harness/mod.rs`,
   `agent.rs::reply`. Immediately visible in the room.
2. Finding 12's logging line, so the rest can be measured.
3. Finding 5 + 6 (richer context, all pending triggers).
4. Finding 7 + 9 + 8 (remove heuristics, structured output, prompt rewrite).
5. Finding 4 (session resume), then 11 and 10.
6. Finding 12's eval, and 13 cleanup along the way.
