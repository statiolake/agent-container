use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "agent-container",
    version,
    about = "Run coding agents inside a Docker sandbox",
    long_about = TOP_LEVEL_HELP,
    after_help = TOP_LEVEL_EXAMPLES,
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Launch a coding agent inside the sandbox container.
    #[command(long_about = RUN_HELP)]
    Run {
        /// Which agent to run as the session's primary binary. Both agents'
        /// auth is still bind-mounted either way, so you can call the other
        /// one from inside.
        #[arg(long, value_enum, default_value_t = AgentKind::Claude)]
        agent: AgentKind,
        /// Rebuild the agent container image before starting, even if it
        /// already exists locally. The proxy image is still built only if
        /// missing.
        #[arg(long)]
        rebuild_image: bool,
        /// Extra arguments forwarded to the chosen agent inside the container.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        passthrough: Vec<String>,
    },

    /// Drop into the container's bash shell for troubleshooting. Uses the
    /// same networking / mounts / auths as `run` but skips the agent
    /// binary so you can poke at the filesystem, curl endpoints, etc.
    #[command(long_about = SHELL_HELP)]
    Shell {
        /// Rebuild the agent container image before starting the shell,
        /// even if it already exists locally.
        #[arg(long)]
        rebuild_image: bool,
        /// Optional command to exec inside bash instead of dropping to a
        /// prompt (e.g. `agent-container shell -- cat /etc/resolv.conf`).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        passthrough: Vec<String>,
    },

    /// Edit agent-container configuration (proxy allowlist, MCP tools).
    ///
    /// Settings are layered: a global file at
    /// `$XDG_CONFIG/agent-container/settings.toml` and a workspace-local
    /// file at `<workspace>/.agent-container/settings.toml`. Both are
    /// merged at runtime; writes go to whichever scope the flags select.
    #[command(long_about = CONFIG_HELP, after_help = CONFIG_EXAMPLES)]
    Config {
        #[command(subcommand)]
        command: Option<ConfigCommands>,
        /// Target the user-global settings file. Mutually exclusive with
        /// --workspace.
        #[arg(long)]
        global: bool,
        /// Target the workspace-local settings file (default).
        #[arg(long, conflicts_with = "global")]
        workspace: bool,
        /// Open the target settings.toml in `$EDITOR` instead of the TUI.
        /// Only meaningful without a subcommand.
        #[arg(long)]
        editor: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AgentKind {
    Claude,
    Codex,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommands {
    /// Print the current settings as TOML. Without flags, prints the
    /// merged view (global ∪ workspace) — which is what the runtime
    /// actually sees.
    #[command(long_about = CONFIG_SHOW_HELP)]
    Show {
        /// Show only the global settings file.
        #[arg(long)]
        global: bool,
        /// Show only the workspace-local settings file.
        #[arg(long, conflicts_with = "global")]
        workspace: bool,
    },
}

const TOP_LEVEL_HELP: &str = r#"Run Claude Code or Codex inside a Docker sandbox around the current workspace.

agent-container keeps the workspace writable, but moves credentials, network
egress, MCP traffic, and host-side tasks through explicit mounts and a host
broker. It is meant for day-to-day coding agent sessions where the practical
blast radius should be the current repository rather than the full host.

Most users start with `agent-container run`. Use `agent-container config` to
edit proxy allow rules, MCP tool policy, and host task-runner commands."#;

const TOP_LEVEL_EXAMPLES: &str = r#"Examples:
  agent-container run
  agent-container run --rebuild-image
  agent-container run --agent codex
  agent-container shell
  agent-container config
  agent-container config show --workspace"#;

const RUN_HELP: &str = r#"Launch a coding agent inside the sandbox container.

The current directory is mounted at the same absolute path inside the
container, so Claude Code resume state stays compatible with native host
runs. The container gets a persistent home under agent-container's data
directory, plus filtered Claude Code and Codex auth/config state from the
host. Network egress goes through the bundled proxy allowlist. Host-only
operations should be exposed through `[task_runner.tasks]` instead of relying
on ordinary container shell access."#;

const SHELL_HELP: &str = r#"Open an interactive shell inside the same container environment used by `run`.

This is useful for debugging mounts, proxy behavior, credentials, or the exact
filesystem view an agent will see. It skips launching Claude Code or Codex."#;

const CONFIG_HELP: &str = r#"Edit agent-container settings.

Settings are TOML and are loaded from two layers:

  Global:    $XDG_CONFIG/agent-container/settings.toml
             Usually ~/.config/agent-container/settings.toml on Linux.

  Workspace: <current-directory>/.agent-container/settings.toml

Runtime behavior uses the merged view: global settings first, then workspace
settings on top. Without `--global`, writes target the workspace file. Use
`--global` for defaults that should apply to every repository.

The workspace `.agent-container` directory is mounted read-only inside the
agent container, so an in-container agent cannot silently rewrite its own
workspace-local policy while running. If you ask an agent to prepare settings,
have it write them on the host side before starting a new `run` session.

Supported TOML shape:

  [proxy]
  allow = ["^api\\.github\\.com$"]

  [mcp.servers.github]
  enabled = true

  [mcp.servers.github.tools]
  list_issues = true
  create_issue = false

  [task_runner.tasks]
  build_image = "docker build -t my-app ."
  deploy = "\"$CONFIG_ROOT/scripts/deploy\" \"$env\""

  [host_fs]
  allow = ["/Users/me/project-notes/**", "!/Users/me/project-notes/secrets/**"]

  [claude]
  tmux_prefix = "C-b"

`proxy.allow` entries are tinyproxy extended regex patterns. MCP server and
tool entries control which host MCP tools are exposed through the broker.
Each task-runner entry becomes an MCP tool that runs on the host; MCP
arguments are passed as environment variables, so a task command can refer to
`$env`, `$value`, and similar names. `$CONFIG_ROOT` points at the host-side
agent-container settings directory that defined the task, so global and
workspace task scripts can use the same command shape.

`host_fs.allow` controls the built-in host-fs MCP server. It is an allowlist
of absolute host-path globs: normal patterns allow access, `!pattern` denies
access, and the default is deny-all as if `!*` had already matched. The
broker also hard-denies common secret file names such as `.env`, `.env.*`,
private key files, `.npmrc`, `.pypirc`, and `.netrc`, even inside an allowed
directory. It reloads this list on every host-fs tool call, so saved settings
take effect without restarting the running agent session.

Claude Code runs inside tmux so agent teams can attach panes. tmux mouse
support is enabled automatically. `claude.tmux_prefix` controls tmux's prefix
key; omit it to keep tmux's default `C-b`, or set it to a tmux key name such
as `C-q`."#;

const CONFIG_EXAMPLES: &str = r#"Examples:
  agent-container config
  agent-container config --global
  agent-container config --workspace --editor
  agent-container config show
  agent-container config show --global
  agent-container config show --workspace"#;

const CONFIG_SHOW_HELP: &str = r#"Print settings as TOML.

Without flags this prints the merged runtime view: global settings plus the
workspace overlay. Use `--global` or `--workspace` to inspect a single file."#;
