# agent-container

Run Claude Code and Codex inside a sandboxed Docker container, with full
network isolation, a host-side proxy allowlist, and a spec-conformant
MCP bridge. The host agents' auth, config, and custom slash commands
carry over so the containerised session feels the same as running them
natively — except an escaped bypass-permissions session or a
prompt-injected shell command cannot reach anything outside the
workspace.

## What it protects against

An agent running with full permissions is a liability. Even if *you*
trust Claude Code or Codex not to be malicious, prompt injection and
destructive tool calls remain real failure modes. `agent-container`
shrinks the blast radius to "whatever is in the current workspace":

- **No host filesystem access** beyond the current working directory
  plus the matching Claude/Codex session history directories.
- **No broad host credentials** — the agent sees only the agent auth it
  needs, not `~/.ssh`, `~/.aws/credentials`, browser cookies, …
- **No direct internet**. The container runs on a `--internal` Docker
  network and reaches the outside world only through a forward proxy
  whose hostname allowlist is under your control.
- **Claude Code's bypass-permissions mode is safe** — it just gives the
  agent full rein inside the container, not on your machine.

## Architecture

```
┌──────── host ──────────────────────────────────────────────────┐
│                                                                │
│  agent-container CLI (Rust)                                    │
│   ├─ materialises Claude Code auth from Keychain into a 0600   │
│   │   file under $XDG_DATA/agent-container/shared/, shared by  │
│   │   concurrent containers via flock — the last container out │
│   │   writes refreshed tokens back and unlinks the shared copy │
│   ├─ bind-mounts host ~/.codex/auth.json directly for Codex    │
│   ├─ bind-mounts host ~/.cursor for Cursor Agent state         │
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
│   │   │  agent    │  claude / codex / cursor, workspace   │    │
│   │   │           │  bind-mount                           │    │
│   │   │           │  HTTPS_PROXY → proxy                  │    │
│   │   └───────────┘                                       │    │
│   └───────────────────────────────────────────────────────┘    │
└────────────────────────────────────────────────────────────────┘
```

The broker also bridges host-side MCP servers into the container as
HTTP. stdio-transport MCPs get writer/reader tasks on the host and
expose their traffic as `POST /mcp/<name>` (client → server) and
`GET /mcp/<name>` (text/event-stream for server-initiated requests like
`roots/list`). The container mounts the workspace at the same absolute
path as the host, keeping Claude Code resume keys stable; spec-defined
URI fields still pass through the path bridge so stdio servers see
coordinates that exist on their side of the bridge.

## Requirements

- macOS with Docker Desktop (primary test target)
- Rust toolchain to build the CLI
- Claude Code, Codex, and/or Cursor Agent installed on the host, already
  logged in (`claude /login`, `codex login`, `cursor agent login`)
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
agent-container run --agent cursor          # Cursor Agent with --yolo
agent-container run --rebuild-image         # rebuild agent-container:dev first
agent-container run --tmux                  # run the agent inside tmux
agent-container run --agent codex -- exec "what does this repo do?"
```

`agent-container run` defaults to Claude Code unless
`[general] default_agent = "codex"` or `"cursor"` is set with
`agent-container config`.
An explicit `--agent` flag always wins over the setting.

Supported agents' auth/state is prepared regardless of which one is the
primary, so a Claude session can call `codex exec …` or `cursor agent …`
as a shell tool and vice versa. In either mode the workspace is the current directory,
mounted at the same absolute path inside the container, and the host
`~/.claude/projects/` tree keeps Claude Code session history visible
even if Claude Code changes how it names per-workspace session
directories. Codex history/resume files under `~/.codex/sessions`,
`~/.codex/archived_sessions`, `~/.codex/shell_snapshots`,
`~/.codex/session_index.jsonl`, and `~/.codex/history.jsonl` are mounted from
the host, so sessions created in the container are visible to host Codex too.
The agent image also includes Python 3 as `python`, plus `openpyxl` for
basic XLSX inspection and generation inside the container.

Cursor Agent runs as `cursor-agent --yolo`. Host `~/.cursor` is mounted at
`/home/agent/.cursor`, with `CURSOR_CONFIG_DIR` and `CURSOR_DATA_DIR` pointing
there. Cursor's macOS CLI stores secret tokens in Keychain, while the Linux
agent reads `$XDG_CONFIG_HOME/cursor/auth.json`; agent-container bridges that
split by materialising a per-run auth.json from the host Keychain and writing
refreshed values back when the last container exits. It also forwards
Cursor-only auth env vars (`CURSOR_API_KEY`, `CURSOR_AUTH_TOKEN`) when they are
explicitly set. The Cursor Agent bundle accepts `--api-key`/`--auth-token` if
you prefer passing credentials for a single run.

The workspace mount is writable, but `<workspace>/.agent-container` is
read-only by default. It is overlaid as read-only at the same path inside
the container and is also covered by the built-in host filesystem readonly
rule, so HostWrite cannot rewrite workspace-local agent-container settings
or scripts. If the directory does not exist on the host, an empty read-only
directory is mounted there so an in-container agent cannot create
workspace-local agent-container settings for itself.

Claude Code and Codex run directly by default. Pass `agent-container run
--tmux` to wrap the agent in tmux so agent teams can attach panes. tmux mouse
support is enabled automatically in that mode. The tmux prefix defaults to
`C-b`; set `[claude] tmux_prefix = "C-q"` in agent-container settings if you
want a different prefix.

### Configure the MCP tool allowlist

```sh
agent-container config mcp
```

A full-screen ratatui UI includes separate MCP tabs for Claude Code and
Codex, with a checkbox per tool:

```
 agent-container  MCP tool allowlist
 notion  github  playwright
────────────────────────────────────────────────────────────────
 ▶ [x] notion-fetch [RO]   Fetch a Notion page
   [ ] notion-create-pages [W]  Create pages
   …
────────────────────────────────────────────────────────────────
 h/l switch tab · j/k move · space toggle · a/A server on/off
 s save · q quit
```

Tools default to the upstream's `readOnlyHint` annotation (read-only on,
destructive off). The allowlist lives in agent-container `settings.toml`:
Claude Code uses `[claude_code.mcp]` for servers from `~/.claude.json`
(top-level plus the current project's entry), Codex uses `[codex.mcp]` for
servers from `~/.codex/config.toml`, and legacy top-level `[mcp]` is still
read as Claude Code policy then migrated on save.

### Drop into the container for troubleshooting

```sh
agent-container shell                       # interactive bash
agent-container shell -- curl -sS "$AGENT_CONTAINER_HOST_ENDPOINT/healthz"
```

Same networking, mounts and auths as `run`, but no agent is started.

## Configuration

### Proxy allowlist

The proxy's filter list is generated at run-time from `proxy.allow` in
`settings.toml` (global + workspace, merged) and bind-mounted into the
container. Entries are tinyproxy extended regex patterns, one per line.

The bundled defaults (see `DEFAULT_ALLOW_ENTRIES` in
`src/proxy_allowlist.rs`) cover the Anthropic / OpenAI APIs, major
package registries (crates.io, registry.npmjs.org, pypi.org, …), apt
repos, the agent broker, and GitHub read-path hosts (`github.com`,
`codeload`, `raw`, release artifacts). `api.github.com` and
`uploads.github.com` are intentionally omitted so a stolen PAT can't
drive destructive REST operations — opt in via
`agent-container config --global` or `--workspace` if you need them.

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

The CLI detects that and serves currently-valid credentials on the
broker's `/aws/credentials` endpoint. The container's Claude Code reads
them on demand through Claude Code's built-in `awsCredentialExport`
hook, pointed at a tiny `curl` against the broker. The container never
writes `~/.aws/credentials`; the credentials live only in Claude Code's
memory for the lifetime of the request.

For one-off sessions, pass `--bedrock-profile PROFILE` instead of changing
host settings or shell env:

```sh
agent-container run --agent claude --bedrock-profile my-bedrock-profile
```

That run injects `CLAUDE_CODE_USE_BEDROCK=1`, `AWS_PROFILE=PROFILE`, and
`AWS_REGION=<region>` into the staged Claude Code settings and the container
process environment. The region comes from `--bedrock-region`, then
`[general].bedrock_region`, and defaults to `ap-northeast-1`:

```toml
[general]
bedrock_region = "ap-northeast-1"
```

On the host, the broker resolves credentials via `aws configure
export-credentials --profile <profile> --format process` — which
handles static keys, SSO and assume-role uniformly. If resolution fails
(typically SSO expired), an optional `awsAuthRefresh` command is run
before a retry. Put it in `~/.claude/settings.json` (preferred) or
`~/.claude.json`:

```json
{
  "awsAuthRefresh": "aws sso login --profile my-bedrock-profile"
}
```

Host-side AWS env vars (`AWS_PROFILE`, `AWS_ACCESS_KEY_ID`,
`AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`) are deliberately **not**
forwarded into the container. `AWS_PROFILE` is set only from settings or
`--bedrock-profile`; otherwise a shell that happens to be pointing at a
different AWS account would silently override the profile you chose.

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

Each `agent-container run` builds a per-run staging tree from a curated slice
of host state, streams it through the built-in Rust tar writer into the
created agent container with `docker cp -`, and bind-mounts only the explicit
host history/auth paths into the container's ephemeral `$HOME`:

- `~/.claude.json` — top-level preferences and the current workspace's
  project entry. Top-level and current-project `mcpServers` are collected
  and rewritten through the agent-container broker; raw `mcpServers` /
  `env` / `hooks` / `permissions` / `sandbox` values are stripped from the
  staged copy. Every other project entry is dropped because their keys are
  host-absolute paths.
- `~/.claude/settings.json` — same stripping.
- `~/.claude/commands/`, `~/.claude/agents/`, `~/.claude/skills/` —
  user-authored slash commands, subagents and skills are mirrored.
- Plugin-provided `commands/` and `skills/` are flattened into those same
  top-level directories. The plugin marketplace/cache tree itself is not
  copied, so Claude Code does not try to refresh marketplaces from inside
  the container.
- `~/.codex/config.toml` — only model/persona scalar settings are inherited,
  while `[mcp_servers.*]` entries are rewritten to the agent-container broker.
  The container pins `approval_policy = "never"` and
  `sandbox_mode = "danger-full-access"` because the container is the sandbox.
  Built-in MCP servers such as `task-runner` and `host-fs` are injected when
  enabled so Codex reaches the same broker tools as Claude Code.
- Codex history state — host `~/.codex/sessions`,
  `~/.codex/archived_sessions`, `~/.codex/shell_snapshots`,
  `~/.codex/session_index.jsonl`, and `~/.codex/history.jsonl` are mounted
  at Codex's normal history paths. Container-created sessions are written
  back to the host history.
- Cursor Agent state/auth — host `~/.cursor` is mounted at `/home/agent/.cursor`
  so CLI config, chats, project trust, plugins and skills are visible to the
  in-container Cursor Agent. Cursor auth is separately mounted at
  `/home/agent/.config/cursor/auth.json`, because that is where the Linux
  Cursor credential manager reads it.

Everything else your agents need is left to the container itself and is
discarded with the container. Host-visible state only persists when it is
listed above or otherwise explicitly bind-mounted.

## Host task runner

Commands configured under `[task_runner.tasks]` are exposed to the agent as
MCP tools named `mcp__task-runner__<task>`. They run on the host, not in the
container, so agents should prefer them whenever a normal shell command
would need host-side capabilities such as starting Docker containers,
touching host-only files, or using network access that is unavailable from
inside the container.

Task tool arguments are passed to the host command as environment variables.
For example, a task command containing `$value` can be called with a MCP
argument named `value`, and the shell will expand it before execution.
Each task also gets `CONFIG_ROOT`, pointing at the host-side
`.agent-container` directory or global config directory that defined that
task. Use it for commands such as `"$CONFIG_ROOT/scripts/foo.sh"` when
global and workspace tasks share the same layout.
The reserved argument `timeout_seconds` sets a per-call task-runner timeout
and is not forwarded to the command environment; omit it to run without a
task-runner timeout.

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
- Claude Code OAuth refresh-token rotation is unavoidable when the
  host app and container both hold independent Keychain-derived
  credentials. Concurrent containers on the same host avoid this by
  sharing one credential file under `$XDG_DATA/agent-container/shared/`;
  a `flock` decides which process writes the refreshed token back to
  Keychain on exit. Codex mounts `~/.codex/auth.json` directly, so host
  and container stay on the same file instead of carrying separate
  refresh-token copies. Cursor's macOS login also uses Keychain, but its
  Linux credential manager reads `auth.json`, so agent-container converts
  between those stores at container start/last exit.

## License

MIT License. See `LICENSE`.
