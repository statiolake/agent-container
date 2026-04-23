use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "agent-container",
    version,
    about = "Run coding agents inside a sandboxed container"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Launch a coding agent inside the sandbox container.
    Run {
        /// Which agent to run as the session's primary binary. Both agents'
        /// auth is still bind-mounted either way, so you can call the other
        /// one from inside.
        #[arg(long, value_enum, default_value_t = AgentKind::Claude)]
        agent: AgentKind,
        /// Extra arguments forwarded to the chosen agent inside the container.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        passthrough: Vec<String>,
    },

    /// Drop into the container's bash shell for troubleshooting. Uses the
    /// same networking / mounts / auths as `run` but skips the agent
    /// binary so you can poke at the filesystem, curl endpoints, etc.
    Shell {
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
    Show {
        /// Show only the global settings file.
        #[arg(long)]
        global: bool,
        /// Show only the workspace-local settings file.
        #[arg(long, conflicts_with = "global")]
        workspace: bool,
    },
}
