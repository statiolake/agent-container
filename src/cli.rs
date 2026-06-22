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
        /// Which agent to run as the session's primary binary. Omit to use
        /// `[general].default_agent` from settings (Claude Code if unset).
        /// Both agents' auth is still bind-mounted either way, so you can
        /// call the other one from inside.
        #[arg(long, value_enum)]
        agent: Option<AgentKind>,
        /// Rebuild the agent container image before starting, even if it
        /// already exists locally. The proxy image is still built only if
        /// missing.
        #[arg(long)]
        rebuild_image: bool,
        /// Run the agent inside tmux. Without this flag, the agent binary is
        /// launched directly.
        #[arg(long)]
        tmux: bool,
        /// Run Claude Code through Bedrock for this session using the named
        /// host AWS profile. This does not modify host env or settings.
        #[arg(long, value_name = "PROFILE")]
        bedrock_profile: Option<String>,
        /// Bedrock AWS region for this Claude Code session. Defaults to
        /// `[general].bedrock_region`, then ap-northeast-1.
        #[arg(long, value_name = "REGION")]
        bedrock_region: Option<String>,
        /// Extra arguments forwarded to the chosen agent inside the container.
        /// Must appear after `--`.
        #[arg(last = true, allow_hyphen_values = true)]
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
        /// prompt. Must appear after `--`.
        #[arg(last = true, allow_hyphen_values = true)]
        passthrough: Vec<String>,
    },

    /// Run a non-interactive command in the container and tear it down.
    #[command(hide = true)]
    Exec {
        /// Rebuild the agent container image before starting, even if it
        /// already exists locally.
        #[arg(long)]
        rebuild_image: bool,
        /// Command to run inside bash. Must appear after `--`.
        #[arg(last = true, allow_hyphen_values = true)]
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

    /// Manage MCP server state used by agent-container.
    Mcp {
        /// Which agent's MCP server declarations to use. Omit to use
        /// `[general].default_agent` from settings.
        #[arg(long, value_enum)]
        agent: Option<AgentKind>,
        #[command(subcommand)]
        command: McpCommands,
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

#[derive(Debug, Subcommand)]
pub enum McpCommands {
    /// List MCP servers declared for the selected agent.
    List,
    /// Authenticate an HTTP MCP server with OAuth 2.1 + PKCE.
    Auth {
        /// MCP server name as declared in the selected agent's config.
        server: String,
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
  agent-container run --agent claude --bedrock-profile sandbox-bedrock
  agent-container run --agent claude --bedrock-profile sandbox-bedrock --bedrock-region us-west-2
  agent-container run --tmux
  agent-container run -- --continue
  agent-container shell
  agent-container shell -- cat /etc/resolv.conf
  agent-container config
  agent-container config show --workspace
  agent-container mcp auth notion
  agent-container mcp --agent codex auth notion"#;

const RUN_HELP: &str = r#"Launch a coding agent inside the sandbox container.

The current directory is mounted at the same absolute path inside the
container, so Claude Code resume state stays compatible with native host
runs. The container home is ephemeral; explicit host state is mounted, while
filtered Claude Code / Codex config generated for this run is streamed into
the created container before startup. Network egress goes through the bundled
proxy allowlist. Host-only
operations should be exposed through `[task_runner.tasks]` instead of relying
on ordinary container shell access.

Arguments for the chosen agent are accepted only after `--`, for example
`agent-container run -- --continue`. Pass `--tmux` when you want the agent
wrapped in a tmux session for agent teams or pane attachment. Pass
`--bedrock-profile PROFILE` with Claude Code to enable Bedrock for only that
session without changing host AWS environment variables. Use `--bedrock-region
REGION` to override `[general].bedrock_region` for that session."#;

const SHELL_HELP: &str = r#"Open an interactive shell inside the same container environment used by `run`.

This is useful for debugging mounts, proxy behavior, credentials, or the exact
filesystem view an agent will see. It skips launching Claude Code or Codex.

Commands to run inside bash are accepted only after `--`, for example
`agent-container shell -- cat /etc/resolv.conf`."#;

const CONFIG_HELP: &str = r#"Edit agent-container settings.

Settings are TOML and are loaded from two layers:

  Global:    $XDG_CONFIG/agent-container/settings.toml
             Usually ~/.config/agent-container/settings.toml on Linux.

  Workspace: <current-directory>/.agent-container/settings.toml

Runtime behavior uses the merged view: global settings first, then workspace
settings on top. Without `--global`, writes target the workspace file. Use
`--global` for defaults that should apply to every repository.

The workspace `.agent-container` directory is read-only inside the agent
container and through HostWrite, so an in-container agent cannot silently
rewrite its own workspace-local policy while running. If you ask an agent to
prepare settings, have it write them on the host side before starting a new
`run` session.

Supported TOML shape:

  [proxy]
  allow = ["^api\\.github\\.com$"]

  [general]
  default_agent = "codex"
  bedrock_region = "ap-northeast-1"

  [claude_code.mcp.servers.github]
  enabled = true

  [claude_code.mcp.servers.github.tools]
  list_issues = true
  create_issue = false

  [codex.mcp.servers.local-tools.tools]
  search = true
  mutate = false

  [task_runner.tasks]
  build_image = "docker build -t my-app ."
  deploy = "\"$CONFIG_ROOT/scripts/deploy\" \"$env\""

  [filesystem]
  mounts = [{ path = "/Users/me/project-notes", readonly = true }]
  hide = ["(^|/)\\.env(\\..*)?$"]
  readonly = ["(^|/)\\.claude(/|$)"]

  [claude]
  tmux_prefix = "C-b"
  skip_bypass_permissions_warning = false

`proxy.allow` entries are tinyproxy extended regex patterns. Claude Code and
Codex have separate MCP policy sections; legacy top-level `[mcp]` is still
read as Claude Code policy and is migrated on the next save.
`general.default_agent` controls which agent `agent-container run` starts when
`--agent` is omitted; explicit `--agent` still wins.
Each task-runner entry becomes an MCP tool that runs on the host; MCP
arguments are passed as environment variables, so a task command can refer to
`$env`, `$value`, and similar names. `$CONFIG_ROOT` points at the host-side
agent-container settings directory that defined the task, so global and
workspace task scripts can use the same command shape. `timeout_seconds` is a
reserved per-call argument that sets a task-runner timeout and is not passed to
the command environment; omit it to run without a task-runner timeout.

`filesystem` controls both container bind mounts and the built-in host-fs MCP
server. The current workspace is always mounted. `filesystem.mounts` adds more
host directories and can mark each one readonly. `filesystem.hide` and `filesystem.readonly` are
regular expressions matched against paths relative to each mounted root:
hidden paths are shadowed so the agent cannot see them, while readonly paths
are visible but overlaid as read-only. Global defaults hide common secret file
names such as `.env`, `.env.*`, private key files, `.npmrc`, `.pypirc`, and
`.netrc`, and make `.agent-container` / `.claude` / `.codex` readonly.
Existing matched files are shadow-mounted at container startup. The host-fs
MCP reloads the policy on every tool call, so saved settings take effect
there without restarting.

Claude Code and Codex run directly by default. Pass `agent-container run
--tmux` to wrap the agent in tmux so agent teams can attach panes. tmux mouse
support is enabled automatically in that mode. `claude.tmux_prefix` controls
tmux's prefix key for agent sessions; omit it to keep tmux's default `C-b`, or
set it to a tmux key name such as `C-q`. Set
`claude.skip_bypass_permissions_warning = true` to pre-acknowledge Claude
Code's bypass-permissions warning inside the staged container settings; the
default is false so Claude Code asks for confirmation."#;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_passthrough_requires_double_dash() {
        let cli = Cli::try_parse_from([
            "agent-container",
            "run",
            "--agent",
            "codex",
            "--",
            "--continue",
            "thread-id",
        ])
        .unwrap();

        let Commands::Run {
            agent,
            tmux,
            passthrough,
            ..
        } = cli.command
        else {
            panic!("expected run command");
        };
        assert_eq!(agent, Some(AgentKind::Codex));
        assert!(!tmux);
        assert_eq!(passthrough, ["--continue", "thread-id"]);

        assert!(Cli::try_parse_from(["agent-container", "run", "--continue"]).is_err());
        assert!(Cli::try_parse_from(["agent-container", "run", "exec", "prompt"]).is_err());
    }

    #[test]
    fn run_tmux_is_a_real_option_before_passthrough_separator() {
        let cli =
            Cli::try_parse_from(["agent-container", "run", "--tmux", "--", "--continue"]).unwrap();

        let Commands::Run {
            tmux, passthrough, ..
        } = cli.command
        else {
            panic!("expected run command");
        };
        assert!(tmux);
        assert_eq!(passthrough, ["--continue"]);
    }

    #[test]
    fn run_bedrock_profile_is_a_real_option() {
        let cli = Cli::try_parse_from([
            "agent-container",
            "run",
            "--agent",
            "claude",
            "--bedrock-profile",
            "sandbox",
            "--",
            "--continue",
        ])
        .unwrap();

        let Commands::Run {
            agent,
            bedrock_profile,
            bedrock_region,
            passthrough,
            ..
        } = cli.command
        else {
            panic!("expected run command");
        };
        assert_eq!(agent, Some(AgentKind::Claude));
        assert_eq!(bedrock_profile.as_deref(), Some("sandbox"));
        assert_eq!(bedrock_region, None);
        assert_eq!(passthrough, ["--continue"]);
    }

    #[test]
    fn run_bedrock_region_is_a_real_option() {
        let cli = Cli::try_parse_from([
            "agent-container",
            "run",
            "--agent",
            "claude",
            "--bedrock-profile",
            "sandbox",
            "--bedrock-region",
            "us-west-2",
        ])
        .unwrap();

        let Commands::Run { bedrock_region, .. } = cli.command else {
            panic!("expected run command");
        };
        assert_eq!(bedrock_region.as_deref(), Some("us-west-2"));
    }

    #[test]
    fn shell_passthrough_requires_double_dash() {
        let cli =
            Cli::try_parse_from(["agent-container", "shell", "--", "cat", "/etc/resolv.conf"])
                .unwrap();

        let Commands::Shell { passthrough, .. } = cli.command else {
            panic!("expected shell command");
        };
        assert_eq!(passthrough, ["cat", "/etc/resolv.conf"]);

        assert!(Cli::try_parse_from(["agent-container", "shell", "cat"]).is_err());
        assert!(Cli::try_parse_from(["agent-container", "shell", "--bogus"]).is_err());
    }

    #[test]
    fn hidden_exec_requires_double_dash_for_command() {
        let cli = Cli::try_parse_from(["agent-container", "exec", "--", "true"]).unwrap();

        let Commands::Exec { passthrough, .. } = cli.command else {
            panic!("expected exec command");
        };
        assert_eq!(passthrough, ["true"]);

        assert!(Cli::try_parse_from(["agent-container", "exec", "true"]).is_err());
        assert!(Cli::try_parse_from(["agent-container", "exec", "--bogus"]).is_err());
    }

    #[test]
    fn mcp_auth_accepts_default_agent_and_server() {
        let cli = Cli::try_parse_from(["agent-container", "mcp", "auth", "notion"]).unwrap();

        let Commands::Mcp { agent, command } = cli.command else {
            panic!("expected mcp command");
        };
        assert_eq!(agent, None);
        let McpCommands::Auth { server } = command else {
            panic!("expected mcp auth command");
        };
        assert_eq!(server, "notion");
    }

    #[test]
    fn mcp_auth_accepts_explicit_agent_and_server() {
        let cli = Cli::try_parse_from([
            "agent-container",
            "mcp",
            "--agent",
            "codex",
            "auth",
            "notion",
        ])
        .unwrap();

        let Commands::Mcp { agent, command } = cli.command else {
            panic!("expected mcp command");
        };
        assert_eq!(agent, Some(AgentKind::Codex));
        let McpCommands::Auth { server } = command else {
            panic!("expected mcp auth command");
        };
        assert_eq!(server, "notion");
    }

    #[test]
    fn mcp_list_accepts_default_agent() {
        let cli = Cli::try_parse_from(["agent-container", "mcp", "list"]).unwrap();

        let Commands::Mcp { agent, command } = cli.command else {
            panic!("expected mcp command");
        };
        assert_eq!(agent, None);
        assert!(matches!(command, McpCommands::List));
    }
}
