use serde_json::{Value, json};

pub const TOOL_NAME: &str = "__agent_container_restart_mcp_server__";

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

pub fn tool_json(server_name: &str, reason: Option<&str>) -> Value {
    json!({
        "name": TOOL_NAME,
        "description": tool_description(server_name, reason),
        "inputSchema": {
            "type": "object",
            "properties": {},
            "additionalProperties": false
        },
        "annotations": {
            "readOnlyHint": false,
            "destructiveHint": false
        }
    })
}

pub fn tools_list_response(id: Value, server_name: &str, reason: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "tools": [tool_json(server_name, Some(reason))]
        }
    })
}

pub fn tool_result_response(id: Value, message: String, is_error: bool) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [{"type": "text", "text": message}],
            "isError": is_error
        }
    })
}
