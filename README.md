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
  plus the matching Claude/Codex session history directories.
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
│   ├─ materialises auth (Keychain / ~/.codex/auth.json) into a  │
│   │   0600 file under $XDG_DATA/agent-container/shared/,       │
│   │   shared by every concurrent container via flock —         │
│   │   the last container out writes refreshed tokens back to   │
│   │   the host and unlinks the shared copy                     │
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
`roots/list`). The container mounts the workspace at the same absolute
path as the host, keeping Claude Code resume keys stable; spec-defined
URI fields still pass through the path bridge so stdio servers see
coordinates that exist on their side of the bridge.

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
agent-container run --rebuild-image         # rebuild agent-container:dev first
agent-container run --agent codex -- exec "what does this repo do?"
```

`agent-container run` defaults to Claude Code unless
`[general] default_agent = "codex"` is set with `agent-container config`.
An explicit `--agent` flag always wins over the setting.

Both agents' auth is bind-mounted regardless of which one is the
primary, so a Claude session can call `codex exec …` as a shell tool
and vice versa. In either mode the workspace is the current directory,
mounted at the same absolute path inside the container, and
`~/.claude/projects/<cwd-encoded>/` keeps the session history on the
host. Codex imports only sessions whose recorded cwd matches the current
workspace into a container-side history tree, so resume/history does not
expose unrelated host projects.

The workspace mount is writable, but `<workspace>/.agent-container` is
overlaid as read-only at the same path inside the container. If the
directory does not exist on the host, an empty read-only directory is
mounted there so an in-container agent cannot create workspace-local
agent-container settings for itself.

Claude Code and Codex run inside tmux so agent teams can attach panes. tmux
mouse support is enabled automatically. The tmux prefix defaults to `C-b`;
set `[claude] tmux_prefix = "C-q"` in agent-container settings if you want a
different prefix.

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
Claude Code uses `[claude_code.mcp]` for servers from `~/.claude.json`,
Codex uses `[codex.mcp]` for servers from `~/.codex/config.toml`, and
legacy top-level `[mcp]` is still read as Claude Code policy then migrated
on save.

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
forwarded into the container. Otherwise a shell that happens to be
pointing at a different AWS account would silently override the profile
you chose in `settings.json`.

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
- Codex history state — matching host sessions are selected by the
  session file's recorded cwd and imported into the container home. The
  container mounts that workspace-specific history view at Codex's normal
  history paths.

Everything else your agents need is left to the container itself. The
persistent home survives across runs, so onboarding prompts and login
state do not recur.

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
- OAuth refresh-token rotation is unavoidable: if you run the host's
  Claude Code (or Codex) at the same time as a container, both sides
  carry an independent copy of the same refresh token, and whichever
  refreshes first invalidates the other. Concurrent containers on the
  same host avoid this by sharing one credential file under
  `$XDG_DATA/agent-container/shared/` (a `flock` decides which process
  writes the refreshed token back to the Keychain / `~/.codex/auth.json`
  on exit), but host-and-container parallel use is not protected — when
  it happens, re-running `claude /login` or `codex login` on the host
  is the recovery path.

## License

Not yet specified. Treat as "all rights reserved" until a `LICENSE`
file is added.

## Acknowledgements

Built with Claude Code on a dogfooding loop — the same `agent-container`
that you see here was the environment that most of this repository was
written in.
