pub const MARKER: &str = "<!-- agent-container environment notice -->";

pub const TEXT: &str = r#"<!-- agent-container environment notice -->

## agent-container Environment

You are running inside an agent-container Docker container. Network access from this container is restricted.

Direct filesystem access outside the current workspace is restricted. To inspect or modify allowed host paths outside this workspace, use the HostRead, HostList, HostWrite, and HostSearch MCP tools instead of ordinary shell commands.

Some host-side operations are exposed through the task_runner MCP server. When an operation cannot run correctly inside the container because it needs host capabilities such as network access, Docker/container lifecycle access, or other host-only tools, consider whether an available task_runner MCP tool should perform that operation on the host.
"#;

pub fn append_to(body: &str) -> String {
    let trimmed = body.trim_end();
    if trimmed.contains(MARKER) {
        format!("{trimmed}\n")
    } else if trimmed.is_empty() {
        TEXT.to_string()
    } else {
        format!("{trimmed}\n\n{TEXT}")
    }
}
