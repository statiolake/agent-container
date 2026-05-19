//! Built-in MCP server that executes user-defined shell commands on the
//! host. Each entry in `settings.toml`'s `[task_runner.tasks]` table
//! becomes a tool; the model can call them via `tools/call` and receives
//! the combined stdout/stderr plus exit code. Tool arguments are passed
//! through as environment variables, so a configured command can use
//! shell expansion such as `$value`.
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
//! - Streaming output. Commands run to completion and the full output
//!   lands in a single JSON-RPC response.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::process::Command;

/// Wire-visible name of the server. Surfaces as the MCP server name both
/// in the container's `~/.claude.json` and in any Claude-Code-side UI
/// (`mcp__task-runner__<tool>`).
pub const NAME: &str = "task-runner";

const PROTOCOL_VERSION: &str = "2024-11-05";

#[derive(Debug, Clone, Default)]
pub struct TaskRunner {
    pub tasks: BTreeMap<String, TaskSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSpec {
    pub command: String,
    pub config_root: PathBuf,
}

impl TaskRunner {
    pub fn new(tasks: BTreeMap<String, TaskSpec>) -> Self {
        Self { tasks }
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
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
                    "tools": { "listChanged": false }
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
                json!({
                    "name": name,
                    "description": format!(
                        "Run on the host via agent-container task-runner: `{cmd}`. Use this instead of ordinary container shell commands when the operation needs host-side capabilities, such as Docker/container lifecycle, host-only files, or network access that the container cannot perform directly. Pass named values as arguments; each key is exposed to the shell as an environment variable, so `$value` expands from an argument named `value`. `$CONFIG_ROOT` points at the agent-container settings directory that defined this task."
                    ),
                    "inputSchema": {
                        "type": "object",
                        "properties": {},
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
                    "unknown task '{name}' — configure it under [task_runner.tasks] in settings.toml"
                ),
            );
        };

        let env_keys: Vec<_> = invocation.env.keys().cloned().collect();
        tracing::info!(task = %name, command = %task.command, config_root = %task.config_root.display(), env_keys = ?env_keys, "task-runner dispatching");
        match run_command(task, &invocation).await {
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
            Err(e) => tool_error(id, format!("task '{name}' failed to spawn: {e:#}")),
        }
    }
}

#[derive(Debug, Default)]
struct CmdInvocation {
    env: BTreeMap<String, String>,
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

fn is_valid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

async fn run_command(task: &TaskSpec, invocation: &CmdInvocation) -> Result<CmdOutput> {
    // Wrap the user's command line in `sh -c` so pipes, quoting, and env
    // expansions behave the way the operator expects when they typed it.
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(&task.command);
    cmd.envs(&invocation.env);
    cmd.env("CONFIG_ROOT", &task.config_root);
    let out = cmd.output().await.context("failed to spawn command")?;
    Ok(CmdOutput {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        code: out.status.code(),
        success: out.status.success(),
    })
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

    fn task(command: impl Into<String>, config_root: impl Into<PathBuf>) -> TaskSpec {
        TaskSpec {
            command: command.into(),
            config_root: config_root.into(),
        }
    }

    fn build() -> TaskRunner {
        let mut tasks = BTreeMap::new();
        tasks.insert("echo".into(), task("echo hi", "/tmp/agent-container-test"));
        tasks.insert("succeed".into(), task("true", "/tmp/agent-container-test"));
        tasks.insert("fail".into(), task("false", "/tmp/agent-container-test"));
        TaskRunner::new(tasks)
    }

    #[tokio::test]
    async fn initialize_returns_server_info() {
        let r = build();
        let req = br#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#;
        let resp = r.handle(req).await.unwrap();
        assert_eq!(resp["result"]["serverInfo"]["name"], NAME);
        assert!(resp["result"]["capabilities"]["tools"].is_object());
    }

    #[tokio::test]
    async fn tools_list_contains_every_task() {
        let r = build();
        let req = br#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#;
        let resp = r.handle(req).await.unwrap();
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
                "printf '%s/%s/%s\\n' \"$value\" \"$count\" \"$enabled\"",
                "/tmp/agent-container-test",
            ),
        );
        let r = TaskRunner::new(tasks);
        let req = br#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"expand","arguments":{"value":"hello world","count":42,"enabled":true}}}"#;
        let resp = r.handle(req).await.unwrap();
        assert_eq!(resp["result"]["isError"], false);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("hello world/42/true"));
    }

    #[tokio::test]
    async fn config_root_points_at_task_settings_root() {
        let mut tasks = BTreeMap::new();
        tasks.insert(
            "root".into(),
            task("printf '%s\\n' \"$CONFIG_ROOT\"", "/tmp/task-root"),
        );
        let r = TaskRunner::new(tasks);
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
