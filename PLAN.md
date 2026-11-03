# nubi-agent: roadmap

The original productization plan (POST format, rate limiting, context, README,
cross-compile, install script) shipped. The current state of the agent and the
reasoning behind its design are in [docs/agent-audit.md](docs/agent-audit.md),
which also records which of its findings have been implemented.

## Next

- Grow `docs/eval/cases.jsonl` from real room situations and run
  `cargo run --example eval` before changing the prompt, model or effort.
- Sessions for Codex (`codex exec resume`) and Gemini, once their CLIs
  expose a stable resume flag.
- Refresh room skills on an engine `skill_update` event instead of only at
  startup.
- The model is the CLI's own unless `--model` is set; gateway users pin
  `claude-opus-4-5` (dashed) until the gateway's upstream serves a newer
  Opus (check with `GET /v1/models` and one real message; the list and the
  upstream disagree today).
- Config file support (`~/.nubi-agent.toml`) for running several agents from
  one definition.
