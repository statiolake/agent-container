//! Built-in MCP server that executes user-defined shell commands on the
//! host. Each entry in `settings.toml`'s `[task_runner.tasks]` table
//! becomes a tool; the model can call them via `tools/call` and receives
//! the combined stdout/stderr plus exit code. Tool arguments are passed
//! through as environment variables, so a configured command can use
//! shell expansion such as `$value`. The reserved `argv` argument is
//! passed as shell positional parameters instead, so commands can forward
//! it with `"$@"`. The reserved `stdin_path` argument can feed a
//! workspace-local file into the command's standard input without forcing
//! the model to paste large content into the conversation.
//! The separate CLI only forwards inherited stdin for tasks whose
//! `allow_stdin` setting is enabled.
//!
//! The broker serves this entirely in-process — there is no upstream
//! process to forward to — so it implements just enough of the MCP
//! JSON-RPC surface (`initialize`, `tools/list`, `tools/call`, plus a few
//! empty-result method stubs) to keep Claude Code happy.
//!
//! Deliberately out of scope for this server:
//!
//! - The regular per-tool allowlist (the user opted in by writing the
//!   task down — making them re-approve the same names in the MCP tab
//!   would just be friction).
//! - MCP streaming output. MCP calls run to completion and the full output
//!   lands in a single JSON-RPC response. The separate in-container CLI
//!   uses the broker's streaming HTTP endpoint.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// Wire-visible name of the server. Surfaces as the MCP server name both
/// in the container's `~/.claude.json` and in any Claude-Code-side UI
/// (`mcp__task-runner__<tool>`).
pub const NAME: &str = "task-runner";
pub const CLI_GUIDANCE: &str = "The container image also includes a `task-runner` CLI for shell pipelines. Invoke `task-runner TASK [KEY=VALUE ...] [-- ARG ...]`; it streams stdin through the host broker only when that task's `allow_stdin` setting is enabled and streams stdout/stderr back. Without that setting the CLI does not read its inherited stdin, so it is safe to call from a `while read` loop. The task-runner MCP server is authoritative for which task names are available, so this CLI cannot execute arbitrary commands or define new tasks.";

const PROTOCOL_VERSION: &str = "2024-11-05";
const TIMEOUT_ARGUMENT: &str = "timeout_seconds";
const ARGV_ARGUMENT: &str = "argv";
const STDIN_PATH_ARGUMENT: &str = "stdin_path";
const MAX_TIMEOUT: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Debug, Clone, Default)]
pub struct TaskRunner {
    pub tasks: BTreeMap<String, TaskSpec>,
    workspace: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSpec {
    pub command: String,
    pub allow_stdin: bool,
    pub config_root: PathBuf,
}

/// A configured task process whose stdin and output streams are owned by the
/// broker's streaming HTTP adapter. The task itself has already gone through
/// the same lookup and argument validation as an MCP `tools/call`.
pub(crate) struct StreamingTask {
    pub(crate) stdin: Option<ChildStdin>,
    pub(crate) stdout: ChildStdout,
    pub(crate) stderr: tokio::process::ChildStderr,
    pub(crate) child: Child,
    pub(crate) timeout: Option<Duration>,
}

pub fn load_specs_from_settings(workspace: &Path) -> Result<BTreeMap<String, TaskSpec>> {
    let global_path = crate::settings::global_path()?;
    let global_root = global_path
        .parent()
        .context("global settings path has no parent")?
        .to_path_buf();
    let global =
        crate::settings::Settings::load_global().context("failed to load global settings")?;

    let workspace_path = crate::settings::workspace_path(workspace);
    let workspace_root = workspace_path
        .parent()
        .context("workspace settings path has no parent")?
        .to_path_buf();
    let workspace_settings = crate::settings::Settings::load_workspace(workspace)
        .context("failed to load workspace settings")?;

    Ok(specs_from_scopes(
        global.task_runner.tasks,
        global_root,
        workspace_settings.task_runner.tasks,
        workspace_root,
    ))
}

fn specs_from_scopes(
    global_tasks: BTreeMap<String, crate::settings::TaskDefinition>,
    global_root: PathBuf,
    workspace_tasks: BTreeMap<String, crate::settings::TaskDefinition>,
    workspace_root: PathBuf,
) -> BTreeMap<String, TaskSpec> {
    let mut tasks = BTreeMap::new();
    for (name, definition) in global_tasks {
        tasks.insert(
            name,
            TaskSpec {
                command: definition.command,
                allow_stdin: definition.allow_stdin,
                config_root: global_root.clone(),
            },
        );
    }
    for (name, definition) in workspace_tasks {
        tasks.insert(
            name,
            TaskSpec {
                command: definition.command,
                allow_stdin: definition.allow_stdin,
                config_root: workspace_root.clone(),
            },
        );
    }
    tasks
}

impl TaskRunner {
    pub fn new(tasks: BTreeMap<String, TaskSpec>, workspace: PathBuf) -> Self {
        Self { tasks, workspace }
    }

    pub(crate) async fn spawn_streaming(
        &self,
        name: &str,
        arguments: Option<&Value>,
    ) -> Result<StreamingTask> {
        let invocation = parse_invocation(arguments).map_err(|message| anyhow!(message))?;
        if invocation.stdin.is_some() {
            bail!(
                "{STDIN_PATH_ARGUMENT} is only available to MCP calls; the task-runner CLI streams its own stdin"
            );
        }
        let task = self
            .tasks
            .get(name)
            .with_context(|| format!("unknown task '{name}'"))?
            .clone();

        let mut cmd = command_for(&task, &invocation);
        if task.allow_stdin {
            cmd.stdin(Stdio::piped());
        } else {
            cmd.stdin(Stdio::null());
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn task '{name}'"))?;
        let stdin = if task.allow_stdin {
            Some(
                child
                    .stdin
                    .take()
                    .context("task process did not expose stdin")?,
            )
        } else {
            None
        };
        let stdout = child
            .stdout
            .take()
            .context("task process did not expose stdout")?;
        let stderr = child
            .stderr
            .take()
            .context("task process did not expose stderr")?;

        Ok(StreamingTask {
            stdin,
            stdout,
            stderr,
            child,
            timeout: invocation.timeout,
        })
    }

    pub(crate) async fn spawn_streaming_cli(
        &self,
        name: &str,
        arguments: &[String],
    ) -> Result<StreamingTask> {
        let arguments = parse_cli_arguments(arguments).map_err(|message| anyhow!(message))?;
        self.spawn_streaming(name, Some(&arguments)).await
    }

    /// Dispatch a JSON-RPC request body. Returns `None` for notifications
    /// (the caller should answer with 202 and an empty body).
    pub async fn handle(&self, body: &[u8]) -> Option<Value> {
        let parsed: Value = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(e) => {
                return Some(parse_error(format!("invalid JSON: {e}")));
            }
        };

        let method = parsed
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let id = parsed.get("id").cloned();

        // Notifications (JSON-RPC requests without an id) never get a
        // response body.
        let Some(id) = id else {
            tracing::debug!(method = %method, "task-runner received notification");
            return None;
        };

        match method.as_str() {
            "initialize" => Some(self.initialize(id)),
            "notifications/initialized" => None,
            "ping" => Some(success(id, json!({}))),
            "tools/list" => Some(self.tools_list(id)),
            "tools/call" => Some(self.tools_call(id, &parsed).await),
            "resources/list" | "resources/templates/list" => {
                Some(success(id, json!({ "resources": [] })))
            }
            "prompts/list" => Some(success(id, json!({ "prompts": [] }))),
            other => Some(method_not_found(id, other)),
        }
    }

    fn initialize(&self, id: Value) -> Value {
        success(
            id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {
                    "tools": { "listChanged": true }
                },
                "serverInfo": {
                    "name": NAME,
                    "version": env!("CARGO_PKG_VERSION"),
                },
            }),
        )
    }

    fn tools_list(&self, id: Value) -> Value {
        let tools: Vec<Value> = self
            .tasks
            .iter()
            .map(|(name, task)| {
                let cmd = &task.command;
                let cli_stdin = if task.allow_stdin {
                    "The CLI may stream its inherited stdin for this task when invoked from a pipe or redirect."
                } else {
                    "The CLI does not read inherited stdin for this task; this keeps it safe to invoke from a `while read` loop."
                };
                json!({
                    "name": name,
                    "description": format!(
                        "Run on the host via agent-container task-runner: `{cmd}`. Use this instead of ordinary container shell commands when the operation needs host-side capabilities, such as Docker/container lifecycle, host-only files, or network access that the container cannot perform directly. Pass named values as arguments; each key is exposed to the shell as an environment variable, so `$value` expands from an argument named `value`. Pass `{ARGV_ARGUMENT}` as an ordered array when the command should receive positional parameters; the shell can forward them with \"$@\". Pass `{STDIN_PATH_ARGUMENT}` to feed a workspace-local file to stdin; relative paths are resolved from the workspace. If the file cannot be read from the shared workspace, move or copy it into the workspace and pass that path. `$CONFIG_ROOT` points at the host-side agent-container settings directory that defined this task. Set `{TIMEOUT_ARGUMENT}` to a positive number of seconds when this host task needs an explicit timeout; omit it to run without a task-runner timeout. {cli_stdin} {CLI_GUIDANCE}"
                    ),
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            TIMEOUT_ARGUMENT: {
                                "type": "number",
                                "exclusiveMinimum": 0,
                                "description": "Optional task-runner timeout in seconds. This reserved argument is not passed to the host command as an environment variable."
                            },
                            ARGV_ARGUMENT: {
                                "type": "array",
                                "items": {
                                    "oneOf": [
                                        { "type": "string" },
                                        { "type": "number" },
                                        { "type": "boolean" }
                                    ]
                                },
                                "description": "Optional ordered positional arguments for the host command. This reserved argument is available to the shell as \"$@\" and is not passed as an environment variable."
                            },
                            STDIN_PATH_ARGUMENT: {
                                "type": "string",
                                "description": "Optional workspace-local file path whose bytes are fed to the host command's standard input. Relative paths are resolved from the workspace. This reserved argument is not passed as an environment variable."
                            }
                        },
                        "additionalProperties": {
                            "oneOf": [
                                { "type": "string" },
                                { "type": "number" },
                                { "type": "boolean" }
                            ],
                            "description": "Named value passed to the host command as an environment variable with the same key."
                        }
                    },
                    // The command is arbitrary shell — never read-only by default.
                    "annotations": { "readOnlyHint": false }
                })
            })
            .collect();
        success(id, json!({ "tools": tools }))
    }

    async fn tools_call(&self, id: Value, req: &Value) -> Value {
        let params = req.get("params");
        let name = params
            .and_then(|p| p.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let arguments = params.and_then(|p| p.get("arguments"));

        let Some(name) = name else {
            return invalid_params(id, "tools/call missing `params.name`");
        };
        let invocation = match parse_invocation(arguments) {
            Ok(invocation) => invocation,
            Err(msg) => return invalid_params(id, &msg),
        };
        let Some(task) = self.tasks.get(&name) else {
            return tool_error(
                id,
                format!(
                    "unknown task '{name}' — it is not currently exposed by the task-runner MCP server"
                ),
            );
        };

        let env_keys: Vec<_> = invocation.env.keys().cloned().collect();
        tracing::info!(task = %name, command = %task.command, config_root = %task.config_root.display(), env_keys = ?env_keys, "task-runner dispatching");
        match run_command(task, &self.workspace, &invocation).await {
            Ok(output) => {
                let text = format_output(&output);
                success(
                    id,
                    json!({
                        "content": [ { "type": "text", "text": text } ],
                        "isError": !output.success,
                    }),
                )
            }
            Err(e) => tool_error(id, format!("task '{name}' failed: {e:#}")),
        }
    }
}

#[derive(Debug, Default)]
struct CmdInvocation {
    env: BTreeMap<String, String>,
    argv: Vec<String>,
    stdin: Option<CmdStdin>,
    timeout: Option<Duration>,
}

#[derive(Debug)]
enum CmdStdin {
    Path(PathBuf),
}

struct CmdOutput {
    stdout: String,
    stderr: String,
    code: Option<i32>,
    success: bool,
}

fn parse_invocation(arguments: Option<&Value>) -> std::result::Result<CmdInvocation, String> {
    let mut invocation = CmdInvocation::default();
    let Some(arguments) = arguments else {
        return Ok(invocation);
    };
    let Some(arguments) = arguments.as_object() else {
        return Err("tools/call `params.arguments` must be an object".to_string());
    };

    for (key, value) in arguments {
        if key == TIMEOUT_ARGUMENT {
            invocation.timeout = Some(parse_timeout(value)?);
            continue;
        }
        if key == ARGV_ARGUMENT {
            invocation.argv = parse_argv(value)?;
            continue;
        }
        if key == STDIN_PATH_ARGUMENT {
            invocation.stdin = Some(CmdStdin::Path(parse_stdin_path(value)?));
            continue;
        }

        if !is_valid_env_key(key) {
            return Err(format!(
                "argument key `{key}` is not a valid environment variable name"
            ));
        }

        let value = match value {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            _ => {
                return Err(format!(
                    "argument `{key}` must be a string, number, or boolean"
                ));
            }
        };
        invocation.env.insert(key.clone(), value);
    }

    Ok(invocation)
}

fn parse_cli_arguments(arguments: &[String]) -> std::result::Result<Value, String> {
    let mut parsed = serde_json::Map::new();
    let mut positional = Vec::new();
    let mut after_separator = false;
    let mut index = 0;

    while index < arguments.len() {
        let argument = &arguments[index];
        if !after_separator && argument == "--" {
            after_separator = true;
            index += 1;
            continue;
        }
        if after_separator {
            positional.push(Value::String(argument.clone()));
            index += 1;
            continue;
        }

        let timeout = if argument == "--timeout-seconds" {
            index += 1;
            arguments
                .get(index)
                .cloned()
                .ok_or_else(|| "--timeout-seconds requires a value".to_string())?
        } else if let Some(value) = argument.strip_prefix("--timeout-seconds=") {
            value.to_string()
        } else {
            let Some((key, value)) = argument.split_once('=') else {
                return Err(format!(
                    "argument `{argument}` must be KEY=VALUE, or placed after `--`"
                ));
            };
            if key == ARGV_ARGUMENT || key == STDIN_PATH_ARGUMENT {
                return Err(format!(
                    "`{key}` is reserved; use `--` for argv or the MCP stdin_path argument"
                ));
            }
            parsed.insert(key.to_string(), Value::String(value.to_string()));
            index += 1;
            continue;
        };
        parsed.insert(TIMEOUT_ARGUMENT.to_string(), Value::String(timeout));
        index += 1;
    }

    if !positional.is_empty() {
        parsed.insert(ARGV_ARGUMENT.to_string(), Value::Array(positional));
    }
    Ok(Value::Object(parsed))
}

fn parse_stdin_path(value: &Value) -> std::result::Result<PathBuf, String> {
    value
        .as_str()
        .map(PathBuf::from)
        .ok_or_else(|| format!("argument `{STDIN_PATH_ARGUMENT}` must be a string"))
}

fn parse_argv(value: &Value) -> std::result::Result<Vec<String>, String> {
    let Some(values) = value.as_array() else {
        return Err(format!("argument `{ARGV_ARGUMENT}` must be an array"));
    };
    values
        .iter()
        .enumerate()
        .map(|(index, value)| match value {
            Value::String(s) => Ok(s.clone()),
            Value::Number(n) => Ok(n.to_string()),
            Value::Bool(b) => Ok(b.to_string()),
            _ => Err(format!(
                "argument `{ARGV_ARGUMENT}` item {index} must be a string, number, or boolean"
            )),
        })
        .collect()
}

fn parse_timeout(value: &Value) -> std::result::Result<Duration, String> {
    let seconds = match value {
        Value::Number(n) => n
            .as_f64()
            .ok_or_else(|| format!("argument `{TIMEOUT_ARGUMENT}` must be a finite number"))?,
        Value::String(s) => s.trim().parse::<f64>().map_err(|_| {
            format!("argument `{TIMEOUT_ARGUMENT}` must be a positive number of seconds")
        })?,
        _ => {
            return Err(format!(
                "argument `{TIMEOUT_ARGUMENT}` must be a positive number of seconds"
            ));
        }
    };
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err(format!(
            "argument `{TIMEOUT_ARGUMENT}` must be a positive number of seconds"
        ));
    }
    if seconds > MAX_TIMEOUT.as_secs_f64() {
        return Err(format!(
            "argument `{TIMEOUT_ARGUMENT}` must be no more than {} seconds",
            MAX_TIMEOUT.as_secs()
        ));
    }
    let duration = Duration::from_secs_f64(seconds);
    Ok(duration)
}

fn is_valid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

async fn run_command(
    task: &TaskSpec,
    workspace: &Path,
    invocation: &CmdInvocation,
) -> Result<CmdOutput> {
    // `allow_stdin` governs only the CLI's inherited process stdin. MCP
    // callers opt in explicitly by supplying `stdin_path`, so that path
    // remains available even for tasks that are safe to call from a shell
    // loop.
    let mut cmd = command_for(task, invocation);
    match &invocation.stdin {
        Some(CmdStdin::Path(path)) => {
            let stdin_path = resolve_workspace_stdin_file(workspace, path)?;
            let file = std::fs::File::open(&stdin_path)
                .with_context(|| format!("failed to open stdin file {}", stdin_path.display()))?;
            cmd.stdin(file);
        }
        None => {
            cmd.stdin(Stdio::null());
        }
    }
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);
    let child = cmd.spawn().context("failed to spawn command")?;
    let out = match invocation.timeout {
        Some(duration) => match tokio::time::timeout(duration, child.wait_with_output()).await {
            Ok(result) => result.context("failed to wait for command")?,
            Err(_) => bail!("command timed out after {} seconds", duration.as_secs_f64()),
        },
        None => child
            .wait_with_output()
            .await
            .context("failed to wait for command")?,
    };
    Ok(CmdOutput {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        code: out.status.code(),
        success: out.status.success(),
    })
}

fn command_for(task: &TaskSpec, invocation: &CmdInvocation) -> Command {
    // Wrap the user's command line in `sh -c` so pipes, quoting, and env
    // expansions behave the way the operator expects when they typed it.
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(&task.command)
        .arg("agent-container-task-runner")
        .args(&invocation.argv);
    cmd.envs(&invocation.env);
    cmd.env("CONFIG_ROOT", &task.config_root);
    cmd.kill_on_drop(true);
    cmd
}

fn resolve_workspace_stdin_file(workspace: &Path, path: &Path) -> Result<PathBuf> {
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.join(path)
    };
    let workspace = workspace
        .canonicalize()
        .with_context(|| format!("failed to resolve workspace {}", workspace.display()))?;
    let candidate = candidate
        .canonicalize()
        .with_context(|| format!("failed to resolve stdin file {}", candidate.display()))?;
    if !candidate.starts_with(&workspace) {
        bail!(
            "stdin_path must point inside the shared workspace so the host-side task-runner can read it (got {}). Move or copy the file into the workspace and pass that shared path instead.",
            candidate.display()
        );
    }
    let meta = std::fs::metadata(&candidate)
        .with_context(|| format!("failed to stat stdin file {}", candidate.display()))?;
    if !meta.is_file() {
        bail!("stdin_path is not a regular file: {}", candidate.display());
    }
    Ok(candidate)
}

fn format_output(o: &CmdOutput) -> String {
    let code = o
        .code
        .map(|c| c.to_string())
        .unwrap_or_else(|| "<signal>".to_string());
    let mut s = format!("exit: {code}\n");
    if !o.stdout.is_empty() {
        s.push_str("--- stdout ---\n");
        s.push_str(&o.stdout);
        if !o.stdout.ends_with('\n') {
            s.push('\n');
        }
    }
    if !o.stderr.is_empty() {
        s.push_str("--- stderr ---\n");
        s.push_str(&o.stderr);
        if !o.stderr.ends_with('\n') {
            s.push('\n');
        }
    }
    if o.stdout.is_empty() && o.stderr.is_empty() {
        s.push_str("(no output)\n");
    }
    s
}

fn success(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn method_not_found(id: Value, method: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": -32601,
            "message": format!("method '{method}' not supported by task-runner"),
        }
    })
}

fn invalid_params(id: Value, msg: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": -32602, "message": msg }
    })
}

fn parse_error(msg: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": Value::Null,
        "error": { "code": -32700, "message": msg }
    })
}

fn tool_error(id: Value, msg: String) -> Value {
    // Surface failures as tool-level errors (isError=true) rather than
    // JSON-RPC errors so the agent sees them as execution failures of
    // the specific tool it called.
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [ { "type": "text", "text": msg } ],
            "isError": true,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn task(command: impl Into<String>, config_root: impl Into<PathBuf>) -> TaskSpec {
        TaskSpec {
            command: command.into(),
            allow_stdin: false,
            config_root: config_root.into(),
        }
    }

    fn task_with_stdin(command: impl Into<String>, config_root: impl Into<PathBuf>) -> TaskSpec {
        TaskSpec {
            command: command.into(),
            allow_stdin: true,
            config_root: config_root.into(),
        }
    }

    fn build() -> TaskRunner {
        let mut tasks = BTreeMap::new();
        tasks.insert("echo".into(), task("echo hi", "/tmp/agent-container-test"));
        tasks.insert("succeed".into(), task("true", "/tmp/agent-container-test"));
        tasks.insert("fail".into(), task("false", "/tmp/agent-container-test"));
        TaskRunner::new(tasks, PathBuf::from("/tmp/agent-container-test-workspace"))
    }

    #[test]
    fn specs_use_host_config_roots_and_workspace_overrides() {
        let mut global = BTreeMap::new();
        global.insert(
            "deploy".to_string(),
            crate::settings::TaskDefinition {
                command: "$CONFIG_ROOT/scripts/deploy".to_string(),
                allow_stdin: false,
            },
        );
        global.insert(
            "global-only".to_string(),
            crate::settings::TaskDefinition {
                command: "global".to_string(),
                allow_stdin: true,
            },
        );

        let mut workspace = BTreeMap::new();
        workspace.insert(
            "deploy".to_string(),
            crate::settings::TaskDefinition {
                command: "$CONFIG_ROOT/scripts/deploy-workspace".to_string(),
                allow_stdin: false,
            },
        );

        let specs = specs_from_scopes(
            global,
            PathBuf::from("/Users/example/.config/agent-container"),
            workspace,
            PathBuf::from("/Users/example/repo/.agent-container"),
        );

        assert_eq!(
            specs["deploy"].config_root,
            PathBuf::from("/Users/example/repo/.agent-container")
        );
        assert_eq!(
            specs["global-only"].config_root,
            PathBuf::from("/Users/example/.config/agent-container")
        );
        assert!(specs["global-only"].allow_stdin);
    }

    #[tokio::test]
    async fn initialize_returns_server_info() {
        let r = build();
        let req = br#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#;
        let resp = r.handle(req).await.unwrap();
        assert_eq!(resp["result"]["serverInfo"]["name"], NAME);
        assert_eq!(resp["result"]["capabilities"]["tools"]["listChanged"], true);
    }

    #[tokio::test]
    async fn tools_list_contains_every_task() {
        let r = build();
        let req = br#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#;
        let resp = r.handle(req).await.unwrap();
        let first = &resp["result"]["tools"].as_array().unwrap()[0];
        assert!(
            first["description"]
                .as_str()
                .unwrap()
                .contains(CLI_GUIDANCE)
        );
        assert!(first["inputSchema"]["properties"][TIMEOUT_ARGUMENT].is_object());
        assert!(first["inputSchema"]["properties"][ARGV_ARGUMENT].is_object());
        assert!(first["inputSchema"]["properties"][STDIN_PATH_ARGUMENT].is_object());
        let names: Vec<_> = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["echo", "fail", "succeed"]);
    }

    #[tokio::test]
    async fn tools_call_runs_successful_task() {
        let r = build();
        let req = br#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"echo"}}"#;
        let resp = r.handle(req).await.unwrap();
        assert_eq!(resp["result"]["isError"], false);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("hi"));
        assert!(text.starts_with("exit: 0"));
    }

    #[tokio::test]
    async fn tools_call_surfaces_nonzero_exit_as_is_error() {
        let r = build();
        let req = br#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"fail"}}"#;
        let resp = r.handle(req).await.unwrap();
        assert_eq!(resp["result"]["isError"], true);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("exit: 1"));
    }

    #[tokio::test]
    async fn tools_call_unknown_task_errors_at_tool_level() {
        let r = build();
        let req = br#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"nope"}}"#;
        let resp = r.handle(req).await.unwrap();
        assert_eq!(resp["result"]["isError"], true);
        assert!(
            resp["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("unknown task 'nope'")
        );
    }

    #[tokio::test]
    async fn named_arguments_are_exposed_as_environment_variables() {
        let mut tasks = BTreeMap::new();
        tasks.insert(
            "expand".into(),
            task(
                "printf '%s/%s/%s/%s\\n' \"$value\" \"$count\" \"$enabled\" \"${timeout_seconds:-unset}\"",
                "/tmp/agent-container-test",
            ),
        );
        let r = TaskRunner::new(tasks, PathBuf::from("/tmp/agent-container-test-workspace"));
        let req = br#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"expand","arguments":{"value":"hello world","count":42,"enabled":true,"timeout_seconds":3}}}"#;
        let resp = r.handle(req).await.unwrap();
        assert_eq!(resp["result"]["isError"], false);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("hello world/42/true/unset"));
    }

    #[tokio::test]
    async fn argv_argument_is_exposed_as_shell_positionals() {
        let mut tasks = BTreeMap::new();
        tasks.insert(
            "argv".into(),
            task(
                "printf '<%s>\\n' \"$@\"; printf 'argv-env=%s\\n' \"${argv:-unset}\"",
                "/tmp/agent-container-test",
            ),
        );
        let r = TaskRunner::new(tasks, PathBuf::from("/tmp/agent-container-test-workspace"));
        let req = br#"{"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"argv","arguments":{"argv":["hello world",42,true]}}}"#;
        let resp = r.handle(req).await.unwrap();
        assert_eq!(resp["result"]["isError"], false);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("<hello world>\n<42>\n<true>"));
        assert!(text.contains("argv-env=unset"));
    }

    #[tokio::test]
    async fn stdin_path_feeds_workspace_file_to_command() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("pr.md"), "# title\nbody\n").unwrap();

        let mut tasks = BTreeMap::new();
        tasks.insert(
            "stdin".into(),
            task(
                "cat; printf 'stdin_path=%s\\n' \"${stdin_path:-unset}\"",
                "/tmp/task-root",
            ),
        );
        let r = TaskRunner::new(tasks, workspace.path().to_path_buf());
        let req = br#"{"jsonrpc":"2.0","id":15,"method":"tools/call","params":{"name":"stdin","arguments":{"stdin_path":"pr.md"}}}"#;
        let resp = r.handle(req).await.unwrap();

        assert_eq!(resp["result"]["isError"], false);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("# title\nbody\n"));
        assert!(text.contains("stdin_path=unset"));
    }

    #[tokio::test]
    async fn stdin_path_must_stay_inside_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();

        let mut tasks = BTreeMap::new();
        tasks.insert("stdin".into(), task("cat", "/tmp/task-root"));
        let r = TaskRunner::new(tasks, workspace.path().to_path_buf());
        let req = format!(
            r#"{{"jsonrpc":"2.0","id":16,"method":"tools/call","params":{{"name":"stdin","arguments":{{"stdin_path":{}}}}}}}"#,
            serde_json::to_string(outside.path().to_str().unwrap()).unwrap()
        );
        let resp = r.handle(req.as_bytes()).await.unwrap();

        assert_eq!(resp["result"]["isError"], true);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("must point inside the shared workspace"));
        assert!(text.contains("Move or copy the file into the workspace"));
    }

    #[tokio::test]
    async fn streaming_task_uses_the_same_invocation_rules() {
        let workspace = tempfile::tempdir().unwrap();
        let mut tasks = BTreeMap::new();
        tasks.insert(
            "stream".into(),
            task_with_stdin(
                "cat; printf '%s:%s\\n' \"$value\" \"$CONFIG_ROOT\" >&2",
                "/tmp/task-root",
            ),
        );
        let runner = TaskRunner::new(tasks, workspace.path().to_path_buf());
        let mut process = runner
            .spawn_streaming_cli(
                "stream",
                &[
                    "value=from-cli".to_string(),
                    "--".to_string(),
                    "unused".to_string(),
                ],
            )
            .await
            .unwrap();

        let mut stdin = process.stdin.take().unwrap();
        stdin.write_all(b"streamed input\n").await.unwrap();
        stdin.shutdown().await.unwrap();
        drop(stdin);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        process.stdout.read_to_end(&mut stdout).await.unwrap();
        process.stderr.read_to_end(&mut stderr).await.unwrap();
        let status = process.child.wait().await.unwrap();

        assert!(status.success());
        assert_eq!(stdout, b"streamed input\n");
        assert_eq!(stderr, b"from-cli:/tmp/task-root\n");
    }

    #[tokio::test]
    async fn streaming_task_without_allow_stdin_uses_null_stdin() {
        let workspace = tempfile::tempdir().unwrap();
        let mut tasks = BTreeMap::new();
        tasks.insert(
            "stream".into(),
            task(
                "if read value; then printf 'received:%s\\n' \"$value\"; else printf 'no-stdin\\n'; fi",
                "/tmp/task-root",
            ),
        );
        let runner = TaskRunner::new(tasks, workspace.path().to_path_buf());
        let mut process = runner.spawn_streaming_cli("stream", &[]).await.unwrap();

        assert!(process.stdin.is_none());
        let mut stdout = Vec::new();
        process.stdout.read_to_end(&mut stdout).await.unwrap();
        let status = process.child.wait().await.unwrap();

        assert!(status.success());
        assert_eq!(stdout, b"no-stdin\n");
    }

    #[tokio::test]
    async fn invalid_argv_is_rejected() {
        let r = build();
        let req = br#"{"jsonrpc":"2.0","id":14,"method":"tools/call","params":{"name":"echo","arguments":{"argv":"not-array"}}}"#;
        let resp = r.handle(req).await.unwrap();
        assert_eq!(resp["error"]["code"], -32602);
        assert!(
            resp["error"]["message"]
                .as_str()
                .unwrap()
                .contains("must be an array")
        );
    }

    #[tokio::test]
    async fn tools_call_times_out_when_requested() {
        let mut tasks = BTreeMap::new();
        tasks.insert(
            "slow".into(),
            task("sleep 5; printf late", "/tmp/agent-container-test"),
        );
        let r = TaskRunner::new(tasks, PathBuf::from("/tmp/agent-container-test-workspace"));
        let req = br#"{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"slow","arguments":{"timeout_seconds":0.01}}}"#;
        let resp = r.handle(req).await.unwrap();
        assert_eq!(resp["result"]["isError"], true);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("timed out"));
    }

    #[tokio::test]
    async fn invalid_timeout_is_rejected() {
        let r = build();
        let req = br#"{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"echo","arguments":{"timeout_seconds":0}}}"#;
        let resp = r.handle(req).await.unwrap();
        assert_eq!(resp["error"]["code"], -32602);
        assert!(
            resp["error"]["message"]
                .as_str()
                .unwrap()
                .contains("positive number")
        );
    }

    #[tokio::test]
    async fn config_root_points_at_task_settings_root() {
        let mut tasks = BTreeMap::new();
        tasks.insert(
            "root".into(),
            task("printf '%s\\n' \"$CONFIG_ROOT\"", "/tmp/task-root"),
        );
        let r = TaskRunner::new(tasks, PathBuf::from("/tmp/agent-container-test-workspace"));
        let req = br#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"root","arguments":{"CONFIG_ROOT":"tool-argument-must-not-win"}}}"#;
        let resp = r.handle(req).await.unwrap();
        assert_eq!(resp["result"]["isError"], false);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("/tmp/task-root"));
        assert!(!text.contains("tool-argument-must-not-win"));
    }

    #[tokio::test]
    async fn invalid_environment_argument_key_is_rejected() {
        let r = build();
        let req = br#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"echo","arguments":{"bad-key":"nope"}}}"#;
        let resp = r.handle(req).await.unwrap();
        assert_eq!(resp["error"]["code"], -32602);
        assert!(
            resp["error"]["message"]
                .as_str()
                .unwrap()
                .contains("not a valid environment variable name")
        );
    }

    #[tokio::test]
    async fn notifications_get_no_response() {
        let r = build();
        // no "id" means it's a notification
        let req = br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        assert!(r.handle(req).await.is_none());
    }

    #[tokio::test]
    async fn unknown_method_is_method_not_found() {
        let r = build();
        let req = br#"{"jsonrpc":"2.0","id":7,"method":"completions/complete"}"#;
        let resp = r.handle(req).await.unwrap();
        assert_eq!(resp["error"]["code"], -32601);
    }
}
