use clap::{Parser, Subcommand};

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
    /// Launch Claude Code inside the sandbox container.
    Run {
        /// Extra arguments forwarded to `claude` inside the container.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        passthrough: Vec<String>,
    },

    /// Edit agent-container configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommands {
    /// Interactively toggle which MCP tools the container can see.
    Mcp,
}
