# Mandates: how skill sets bind an agent, end to end

Written 2026-11-02 from the engine (`~/nubi`, `origin/main` at `632003919`;
the local checkout `fix/search-api` predates the feature), the agent
(`~/nubi-agent`, branch `feat/smarter-agent`) and the desktop client
(`~/nubi-desktop`). Line references are to those revisions.

## Origin

The desktop plan (`~/nubi-desktop/docs/plans/skill-sets.md`) records the
brief in the user's words: "an entity that holds skills and can be attached
to agent and they always obey it." Before it, an agent "had" a skill only by
being a member of its room, nothing made it follow one, and nubi-agent's
room-skill loader did not even parse the engine's response. Skill sets are
the entity, the mandate is the obligation, and the hash is how "always obey"
becomes checkable. The engine work landed as nubi PR #63 (`afe3244d5`), the
agent side as nubi-agent PR #19, the UI as nubi-desktop PR #134.

## One paragraph

A **skill** is a room whose owner's messages are prompt versions. A **skill
set** is a room of kind `skill_set` holding an ordered list of skills, each
marked required or suggested, optionally pinned to a version. **Attaching**
a set to an agent (up to eight per agent) makes its skills part of that
agent's **mandate**: the resolved, ordered, deduplicated list of skills the
agent must follow, served by `GET /agents/{agent}/mandate` together with a
**hash** of the required skills' content. Whenever anything that feeds the
mandate changes, the engine re-resolves it and pushes `mandate_update
{agent_id, revision, hash}` on the space SSE stream. The agent fetches the
mandate, puts it at the top of its system prompt, recomputes the hash from
what it injected, refuses to answer if the two disagree, and stamps every
reply with `metadata.mandate_hash`. In **strict mode** the engine rejects a
reply whose stamp is not the current hash (409 `mandate_stale`), which
proves the agent spoke under the current mandate. Under the **mandate_only**
policy the engine also hides every other skill from the agent.

## The engine side (`~/nubi`)

### Entities

| Thing | Where | Shape |
|---|---|---|
| Skill set | `src/types/room_meta.rs:143-185` | `RoomKind::SkillSet(SkillSetMeta { owner, description, status, skills: Vec<SkillRef>, revision, updated_at_ms })`; `SkillRef { room_id, pin: Option<u64>, required: bool }` |
| Attachment ("mandate row") | `src/storage/meta/mandates.rs:29` | `(space, agent, set) → { position, attached_by, attached_at }`, plus a reverse index by set. Cap `MAX_SETS_PER_AGENT = 8` |
| Announced state | `src/storage/meta/mandates.rs:41` | per agent: `{ revision, hash }` of the last `mandate_update` |
| Agent policy | `src/types/member.rs:133-151` | on the agent card: `skills_policy: mandate_first \| mandate_only`, `enforce_mandate: bool` (both default off) |
| Feature flag | `src/config.rs:89`, deploy workflows | `features.skill_sets` gates *creating* sets only; reading, attaching and resolving always work, so an engine without the flag just serves empty mandates |

### Resolution (`src/http/mandate.rs:compute`)

1. One store transaction reads the agent card, its attachments in position
   order, every set room and every skill room they reference.
2. A synchronous walk over sets in attachment order, then skills in set
   order, decides what is served and what is only warned about: a missing or
   non-set room, an archived set, an archived or ownerless skill. The walk
   order, not I/O order, fixes the final ordering.
3. Content is read concurrently for every candidate occurrence: the pinned
   version, else the skill's `active_version`, else the owner's latest
   message. The message must be the owner's, or the skill is omitted with a
   warning.
4. Dedup by room id, **first served occurrence wins**: if an earlier set's
   occurrence was omitted (archived, no content, bad pin), a later set's
   occurrence stands in, pin and all. A repeat with a different pin adds a
   warning.
5. Output: `ResolvedMandate { hash, revision, policy, sets, skills,
   warnings }` (`src/domain/mandate.rs:17`).

Nothing in `compute` writes. `resolve_mandate` is the read-only entry;
`refresh_mandate(force, also_notify_sets)` additionally records the hash and
announces.

### The hash (`src/domain/mandate.rs:72-104`)

`sha256:` + hex SHA-256 over compact JSON with sorted keys:

```
[{"content_sha256":"<hex of content bytes>","room_id":"<id>","version_index":<n>}, …]
```

Required skills only, in resolution order; suggested skills do not affect
it. Content is hashed exactly as served, untrimmed. An empty mandate hashes
`[]`. The same bytes are what nubi-agent recomputes (`src/mandate.rs:98-125`
there), and the engine's unit tests pin test vectors for both.

### Announcing changes

`record_mandate_hash(hash, force)` bumps the per-agent `revision` when the
hash differs from the stored one, or always when `force` is set. Then
`mandate_update {agent_id, revision, hash}` goes out on the space stream to
the agent itself and to the active members of its sets (so dashboards
refresh), regardless of `mentioned_only`. Channel capacity 1024; a lagging
subscriber increments `nubi_mandate_update_lagged_total`.

| Trigger | Entry point | Force |
|---|---|---|
| attach, detach, reorder | `skill_sets.rs` handlers | yes |
| set edited (skills, name, status), set archived or deleted | `update_skill_set`, `delete_skill_set` → `announce_set_changed` | yes; archive/delete also detach everywhere first |
| policy change | `set_mandate_policy` | yes |
| skill owner posts a new version, active version or status changes, skill deleted | `messages.rs:174-179` → `spawn_skill_changed` | no: announces only where the hash moved |
| agent deleted | `announce_agent_deleted` | members of its former sets are told; revision 0, empty hash |
| strict-mode mismatch | `require_current_mandate` | records and announces only if the stored hash was stale |

### API (`src/http/routes.rs:255-293`, handlers in `skill_sets.rs`)

```
POST   /skill-sets                          create (gated by features.skill_sets; caller becomes Owner)
GET    /skill-sets[?status=]                sets the caller is a member of or attached to
GET    /skill-sets/{set}                    set + skill names/statuses, no content
PATCH  /skill-sets/{set}                    edit name/description/status/skills (Owner/Admin); bumps revision
DELETE /skill-sets/{set}                    soft-delete (Owner), detaches everywhere
GET    /skill-sets/{set}/revisions          skill_set_revision events from the set's log
GET    /skill-sets/{set}/agents             attachments
PUT    /skill-sets/{set}/agents/{agent}     attach or move ({"position": n}); 201 when new
DELETE /skill-sets/{set}/agents/{agent}     detach
GET    /agents/{agent}/skill-sets           the agent's sets in resolution order
PUT    /agents/{agent}/skill-sets/order     bare array or {"set_ids": [...]} listing exactly the attached sets
GET    /agents/{agent}/mandate              resolved mandate; ETag = quoted hash; If-None-Match → 304
PUT    /agents/{agent}/mandate-policy       {skills_policy?, enforce_mandate?} (admin key only)
```

Authorization (`src/domain/authz.rs:180`, `skill_sets.rs:597-614`): nobody
writes to a set's log (it holds only engine-written revision events).
Owner/Admin of the set may edit, attach, detach and invite; only the Owner
may delete. **An agent can never attach or detach a set on itself**; the
admin key can do anything. Reading a mandate is allowed to the agent itself,
the admin key, or an Owner/Admin of any attached set. A published set may
only list published skills; a skill archived later stays and resolves to a
warning. Attaching to an archived set is 409 `skill_set_archived`; a ninth
set is 409 `too_many_skill_sets`; an order that does not list exactly the
attached sets is 400, and an empty order needs the admin key (review fix:
otherwise any agent could force a `mandate_update` for any set-less agent).

### Enforcement

**Strict mode** (`enforce_mandate`, `src/http/handlers/messages.rs:40-64`):
on `POST /messages`, after the idempotent-duplicate check (a retry of a
message that went through under an older mandate must not become a 409),
the sender's card is read; if strict, `metadata.mandate_hash` must equal the
freshly resolved hash, else 409 `mandate_stale` with `current_hash` in the
error body. A match is checked read-only; only a mismatch goes through
`refresh_mandate`, so the 409's hash and the agent's next `mandate_update`
agree.

**mandate_only** (`skills_policy`, `src/http/handlers/skills.rs:181-193`):
`/skills/{room}/prompt` and `/versions` answer 403 `outside_mandate` for a
skill not in the agent's mandate, and `GET /rooms` drops those skill rooms
from the agent's listing. The default `mandate_first` imposes no
restriction; it is a hint to clients to put the mandate first.

## The agent side (`~/nubi-agent`)

| Concern | Where |
|---|---|
| Wire types, hash, prompt section | `src/mandate.rs`: `Mandate`, `MandateSkill`, `mandate_hash` (byte-identical canonical form), `build_mandate_section` |
| Fetch and cache | `MandateLoader::fetch(conditional)` sends `If-None-Match` with the cached ETag; 304 → `Unchanged`, 200 → `Updated`, 404 → `NoMandate` (never had one) or `Gone` (had one; keep it, mark unverified), unparsable body → keep the last good one |
| Status machine | `agent.rs` `MandateStatus { known, dirty, announced, unverified }` |
| Startup | `init_mandate`: fetch with backoff; without `--mandate-strict` three attempts then carry on, with it retry forever |
| SSE | `mandate_update` for this agent id → mark dirty, remember the announced hash, conditional refetch off the SSE loop |
| Before every turn | `check_mandate`: conditional refetch; if the injected hash is not the server's or an announced hash is outstanding, one unconditional refetch; still off → refuse the turn and log why |
| Injection | `inject_mandate(mandate, adhoc_skills)`: the mandate section sits right after the identity line, above `--system-prompt` and ad-hoc skills, so persona text cannot override it. For CLIs that read the system prompt from a file (Gemini, OpenCode) the file is rewritten first and the swap is committed only if the write succeeded |
| Hash recomputed, not trusted | `CliRuntime::mandate_hash` recomputes from the injected `skills`, so an edited or dropped skill shows as a mismatch; the reply is stamped with the hash of the prompt the run actually used, read under the same lock |
| Stamping | every post carries `metadata.mandate_hash` (`nubi.rs: post_metadata`) |
| Ad-hoc skills vs mandate | `adhoc_skills()`: a file or room skill that collides with a mandated one by room id or name is dropped (a local file cannot shadow a mandate); under `mandate_only` or `--no-local-skills` file skills are dropped entirely |
| Strictness knob | `--mandate-strict`: refuse turns while no mandate was ever fetched or the held one cannot be verified (404/garbage after a good one). Off by default because an engine without the feature is the common case |

The tests in `agent.rs` cover the races that matter: a stale in-flight fetch
cannot clear `dirty`; a failed injection keeps the old mandate and refuses
the turn; a reply is stamped with the hash of the prompt it ran with even if
a refresh landed mid-run; strict mode behaviour on 404 and on garbage.

## The desktop side (`~/nubi-desktop`)

`src/views/skill-set-editor.tsx` edits sets; `src/components/
agent-mandate-panel.tsx` shows an agent's resolved mandate and lets an
Owner/Admin attach, detach and reorder; `src/lib/sse-manager.tsx` consumes
`mandate_update` to refresh. The plan is `docs/plans/skill-sets.md`.

## How a change flows, start to finish

1. A skill owner posts a new version in skill room `refund-9x2k`.
2. The engine's `append_message` sees an owner message in a skill room and
   spawns `announce_skill_changed`: every agent bound through a set that
   lists `refund-9x2k` is re-resolved; where the hash moved, the revision is
   bumped and `mandate_update` is sent to the agent and the sets' members.
3. nubi-agent receives it on the space stream, marks the mandate dirty with
   the announced hash, and refetches with `If-None-Match`. A 200 replaces
   the cached mandate, the system prompt is re-rendered (and rewritten on
   disk for file-based CLIs), and the status returns to clean only once the
   injected hash equals the announced one.
4. On the next message, `check_mandate` confirms sync (or does one more
   unconditional refetch), the turn runs, and the reply is posted with
   `metadata.mandate_hash = sha256:…`.
5. If the agent is in strict mode and the stamp is stale (the refetch failed,
   or a change landed between check and post), the engine answers 409
   `mandate_stale` with the current hash; the agent's post retry logic does
   not retry 4xx, so the turn is logged as failed and the next turn refetches.

## Observations

- **The hash is the contract.** Both sides implement the same canonical
  bytes independently and test them against fixed vectors. Keep it that way:
  any change to the canonical form has to land on both sides at once.
- **Suggested skills are not hashed.** Strict mode therefore proves the
  agent saw the *required* skills; a suggested skill can change without a
  `mandate_update` being required for correctness (it is still announced
  when attached sets change, via `force`).
- **The feature flag only gates creation.** Production and staging pass
  `NUBI_FEATURES_SKILL_SETS` from a repository variable defaulting to
  `false`; with it off nobody can create a set, but the resolve endpoint
  works and returns an empty mandate, which is why nubi-agent treats 404 as
  "no mandate" rather than an error.
- **Two places can still disagree briefly.** Between nubi-agent's
  `check_mandate` and its `post`, a skill change can land; the reply is then
  stamped with the hash of the prompt it ran with, which strict mode will
  reject. That is the intended semantics (the reply was written under an
  outdated mandate), and the cost is one failed turn.
- **No room-skill refresh on `skill_update`.** nubi-agent reloads skill
  rooms only at startup; mandated skills are the only ones that follow live
  changes. Noted in `PLAN.md` as a next step.
- **mandate_only is enforced server-side,** not just by the agent dropping
  local files: the prompt and version endpoints refuse, and listings hide
  the rooms. The agent-side `--no-local-skills` is the client twin for
  skills that never touch the engine (files on disk).
