pub const TOOL_NAME: &str = "agent_container_restart_mcp";

pub fn tool_description(server_name: &str, reason: Option<&str>) -> String {
    let mut description = format!(
        "Restart and reinitialize MCP server '{server_name}' after refreshing host-side credentials."
    );
    if let Some(reason) = reason.filter(|s| !s.trim().is_empty()) {
        description.push_str(" Last error: ");
        description.push_str(reason.trim());
    }
    description
}
