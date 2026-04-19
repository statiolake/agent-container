# agent-container

Run Claude Code and Codex inside a sandboxed Docker container, with full
network isolation, a host-side proxy allowlist, and a spec-conformant
MCP bridge. The host agents' auth, config, and custom slash commands
carry over so the containerised session feels the same as running them
natively — except an escaped `--dangerously-skip-permissions` or a
prompt-injected shell command cannot reach anything outside the
workspace.

## What it protects against

An agent running with full permissions is a liability. Even if *you*
trust Claude Code or Codex not to be malicious, prompt injection and
destructive tool calls remain real failure modes. `agent-container`
shrinks the blast radius to "whatever is in the current workspace":

- **No host filesystem access** beyond the current working directory
  plus the matching `~/.claude/projects/<cwd>` session history
  directory.
- **No host credentials** — the agent sees only the bind-mounted OAuth
  tokens, not `~/.ssh`, `~/.aws/credentials`, browser cookies, …
- **No direct internet**. The container runs on a `--internal` Docker
  network and reaches the outside world only through a forward proxy
  whose hostname allowlist is under your control.
- **`--dangerously-skip-permissions` is safe** — it just gives the
  agent full rein inside the container, not on your machine.

## Architecture

```
┌──────── host ──────────────────────────────────────────────────┐
│                                                                │
│  agent-container CLI (Rust)                                    │
│   ├─ extracts auth (Keychain / ~/.codex/auth.json) to a 0600   │
│   │   temp file, deleted on exit                               │
│   ├─ spawns a broker HTTP server on 127.0.0.1:<random>         │
│   │   serving /aws/credentials + /mcp/<name>/...               │
│   └─ runs `docker compose -p agent-container-<pid>` with:      │
│                                                                │
│   ┌────────────── compose project ───────────────────────┐     │
│   │                                                       │    │
│   │  [egress bridge] ─── internet ──────→                 │    │
│   │     ↑                                                 │    │
│   │     │                                                 │    │
│   │   ┌─┴─────────┐  tinyproxy with hostname allowlist    │    │
│   │   │  proxy    │  (api.anthropic.com, chatgpt.com,     │    │
│   │   │           │  crates.io, registry.npmjs.org, …)    │    │
│   │   └─┬─────────┘                                       │    │
│   │     │                                                 │    │
│   │   [jail bridge, --internal]                           │    │
│   │     │                                                 │    │
│   │   ┌─┴─────────┐                                       │    │
│   │   │  agent    │  claude / codex, workspace bind-mount │    │
│   │   │           │  HTTPS_PROXY → proxy                  │    │
│   │   └───────────┘                                       │    │
│   └───────────────────────────────────────────────────────┘    │
└────────────────────────────────────────────────────────────────┘
```

The broker also bridges host-side MCP servers into the container as
HTTP. stdio-transport MCPs get writer/reader tasks on the host and
expose their traffic as `POST /mcp/<name>` (client → server) and
`GET /mcp/<name>` (text/event-stream for server-initiated requests like
`roots/list`). Spec-defined URI fields are translated between the
container's `/workspace` and the host's real path so the stdio server
sees coordinates that actually exist on its side of the bridge.

## Requirements

- macOS with Docker Desktop (primary test target)
- Rust toolchain to build the CLI
- Claude Code and/or Codex installed on the host, already logged in
  (`claude /login`, `codex login`)
- `aws` CLI on `PATH` if you use the Bedrock pathway

Linux with native Docker probably works too — `host.docker.internal` is
created via `--add-host=host.docker.internal:host-gateway`, which works
on recent Docker versions. Untested.

## Install

```sh
git clone https://github.com/statiolake/agent-container.git
cd agent-container
cargo install --path .
```

The container images (`agent-container:dev` and
`agent-container-proxy:dev`) are built automatically on first run.

## Usage

### Launch an agent

```sh
agent-container run                         # Claude Code
agent-container run --agent codex           # Codex
agent-container run --agent codex -- exec "what does this repo do?"
```

Both agents' auth is bind-mounted regardless of which one is the
primary, so a Claude session can call `codex exec …` as a shell tool
and vice versa. In either mode the workspace is the current directory
(mounted at `/workspace`), and `~/.claude/projects/<cwd-encoded>/`
keeps the session history on the host.

### Configure the MCP tool allowlist

```sh
agent-container config mcp
```

A full-screen ratatui UI with one tab per MCP server and a checkbox per
tool:

```
 agent-container  MCP tool allowlist
 notion  github  playwright
────────────────────────────────────────────────────────────────
 ▶ [x] notion-fetch [RO]   Fetch a Notion page
   [ ] notion-create-pages [W]  Create pages
   …
────────────────────────────────────────────────────────────────
 h/l switch MCP · j/k move · space toggle · a/A tab on/off
 s save · q quit
```

Tools default to the upstream's `readOnlyHint` annotation (read-only on,
destructive off). The allowlist lives at
`$XDG_CONFIG/agent-container/mcp.toml` and can be edited by hand.

### Drop into the container for troubleshooting

```sh
agent-container shell                       # interactive bash
agent-container shell -- curl -sS "$AGENT_CONTAINER_HOST_ENDPOINT/healthz"
```

Same networking, mounts and auths as `run`, but no agent is started.

## Configuration

### Proxy allowlist

`docker/proxy/allowlist.txt` is bind-mounted into the proxy container at
start-up. Edit it and restart the run; the file is a tinyproxy filter
list (extended regex, one pattern per line). The defaults cover the
Anthropic / OpenAI APIs, major package registries (crates.io,
registry.npmjs.org, pypi.org, …), GitHub, apt repos and the agent
broker.

### Bedrock

Put this in `~/.claude/settings.json` on the host:

```json
{
  "env": {
    "CLAUDE_CODE_USE_BEDROCK": "1",
    "AWS_PROFILE": "my-bedrock-profile",
    "ANTHROPIC_MODEL": "anthropic.claude-sonnet-4-20250514-v1:0",
    "AWS_REGION": "us-west-2"
  }
}
```

The CLI detects that, resolves credentials via `aws configure
export-credentials --profile <profile> --format process` (which handles
static keys, SSO, and assume-role uniformly), and serves them on the
broker's `/aws/credentials` endpoint. The container's Claude Code reads
them on demand via a short shell script set as `awsAuthRefresh`, so
long-running sessions keep working after SSO session refreshes.

If you want an SSO refresh command to run automatically when credential
resolution fails on the host, set it in `~/.claude.json`:

```json
{
  "awsAuthRefresh": "aws sso login --profile my-bedrock-profile"
}
```

### Logging

Broker and CLI diagnostics go to stderr by default. For a clean log
without fighting the container's TUI:

```sh
AGENT_CONTAINER_LOG_FILE=/tmp/agent-container.log \
RUST_LOG=agent_container=debug \
  agent-container run
```

Then `tail -f /tmp/agent-container.log` in another terminal.

## Host configuration inheritance

Each `agent-container run` copies a curated slice of host state into the
container's persistent `$HOME` (kept at
`$XDG_DATA/agent-container/home`, separate from your real `~`):

- `~/.claude.json` — top-level preferences and the current workspace's
  project entry, with `mcpServers` / `env` / `hooks` / `permissions` /
  `sandbox` stripped. Every other project entry is dropped because
  their keys are host-absolute paths.
- `~/.claude/settings.json` — same stripping.
- `~/.claude/commands/`, `~/.claude/agents/`, `~/.claude/skills/`,
  `~/.claude/plugins/` — mirrored verbatim so custom slash commands,
  subagents, user-authored skills and marketplace plugins all work.
- `~/.codex/config.toml` — only `model`, `model_reasoning_effort`,
  `personality`, plus pinned `approval_policy = "never"` and
  `sandbox_mode = "danger-full-access"` (the container is the sandbox;
  Codex's own bubblewrap can't nest).

Everything else your agents need is left to the container itself. The
persistent home survives across runs, so onboarding prompts and login
state do not recur.

## Known limitations

- macOS/Docker Desktop is the primary test target. Linux with native
  Docker should work but is untested; `--internal` networks behave
  slightly differently between the two.
- Windows is not supported — the path translation assumes POSIX paths.
- MCP `sampling/createMessage` and `elicitation/create` server-initiated
  requests are not yet forwarded to the client (only the URI-bearing
  methods are spec-translated). If you hit a server that requires them
  please file an issue.
- The container runs as the host user's UID/GID (so bind-mounted files
  get the right ownership). The in-container bash has no matching entry
  in `/etc/passwd`, so interactive shells greet you with `I have no
  name!`. Cosmetic only.

## License

Not yet specified. Treat as "all rights reserved" until a `LICENSE`
file is added.

## Acknowledgements

Built with Claude Code on a dogfooding loop — the same `agent-container`
that you see here was the environment that most of this repository was
written in.
