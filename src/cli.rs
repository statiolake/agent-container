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

    /// Edit agent-container configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AgentKind {
    Claude,
    Codex,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommands {
    /// Interactively toggle which MCP tools the container can see.
    Mcp,
}
