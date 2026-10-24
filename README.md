# nubi-agent

A CLI tool that runs Claude Code agents responding to nubi room messages.

## Installation

```bash
cargo install --path .
```

## Usage

```bash
nubi-agent \
  --url http://localhost:9099 \
  --space your-space-id \
  --agent your-agent-id \
  --token your-nubi-token
```

## Options

| Flag | Env | Default | Description |
|------|-----|---------|-------------|
| `--url` | `NUBI_URL` | `http://localhost:9099` | Nubi server URL |
| `--space` | `NUBI_SPACE` | required | Space ID |
| `--agent` | `NUBI_AGENT` | required | Agent ID (your identity) |
| `--token` | `NUBI_TOKEN` | required | API token |
| `--rooms` | - | `*` | Room(s) to listen to, or `*` for all |
| `--system-prompt` | - | - | Custom system prompt for Claude |
| `--skip-permissions` | - | false | Skip Claude permission prompts |
| `--context-size` | - | `10` | Number of recent messages as context |
| `--rate-limit-ms` | - | `5000` | Rate limit between responses (ms) |
| `--mcp-url` | `NUBI_MCP_URL` | - | MCP server URL for cross-room messaging |
| `--heartbeat-secs` | - | `30` | Heartbeat interval during long tasks (0 to disable) |

## Features

### Heartbeat During Long Tasks

When Claude is processing a request (especially with tool use), the agent posts periodic status updates to let users know it's still working:

- Default interval: 30 seconds
- Messages rotate: "still working on it...", "thinking...", "processing...", "hang on..."
- Disable with `--heartbeat-secs 0`

### MCP Integration

Enable cross-room messaging by providing an MCP server URL:

```bash
nubi-agent \
  --mcp-url http://localhost:8089/mcp \
  ...
```

This generates a temporary MCP config at `/tmp/mcp-{agent_id}.json` and passes it to Claude.

### Rate Limiting

Prevents the agent from responding too frequently in the same room. Default is 5 seconds between responses per room.

### Context Window

Includes recent messages from the room as context for Claude. Default is 10 messages.

## Behavior

- Responds to @mentions in group rooms
- Always responds in DMs
- Skips own messages and historical messages
- Filters out short confirmations ("ok", "done", etc.)
- Strips `<thinking>` tags from Claude's output
