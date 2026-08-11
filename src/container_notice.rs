pub const MARKER: &str = "<!-- agent-container environment notice -->";

const BASE_TEXT: &str = r#"<!-- agent-container environment notice -->

## agent-container Environment

You are running inside an agent-container Docker container. Network access from this container is restricted.

Direct filesystem access outside the current workspace is restricted. To inspect or modify allowed host paths outside this workspace, use the HostRead, HostList, HostWrite, and HostSearch MCP tools instead of ordinary shell commands.

Some host-side operations are exposed through the task_runner MCP server. When an operation cannot run correctly inside the container because it needs host capabilities such as network access, Docker/container lifecycle access, or other host-only tools, consider whether an available task_runner MCP tool should perform that operation on the host.

"#;

fn text() -> String {
    format!("{BASE_TEXT}\n{}", crate::task_runner::CLI_GUIDANCE)
}

pub fn append_to(body: &str) -> String {
    let trimmed = body.trim_end();
    let text = text();
    if trimmed.contains(MARKER) {
        if trimmed.contains(crate::task_runner::CLI_GUIDANCE) {
            format!("{trimmed}\n")
        } else {
            format!("{trimmed}\n\n{}\n", crate::task_runner::CLI_GUIDANCE)
        }
    } else if trimmed.is_empty() {
        text
    } else {
        format!("{trimmed}\n\n{text}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_cli_guidance_to_an_existing_notice() {
        let old_notice = format!("{MARKER}\n\nold notice");
        let out = append_to(&old_notice);

        assert!(out.contains("old notice"));
        assert!(out.contains(crate::task_runner::CLI_GUIDANCE));
    }
}
