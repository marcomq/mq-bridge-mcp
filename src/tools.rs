use rmcp::model::Tool;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConnectorRef {
    pub connector_type: String,
    /// The original endpoint name as defined in config (not the tool name).
    pub endpoint_name: Option<String>,
}

#[derive(Clone, Debug)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    pub action: McpToolAction,
    pub connector: ConnectorRef,
    pub schema: Option<serde_json::Value>,
}

#[derive(Clone, Debug)]
pub enum McpToolAction {
    Publish,
    Introspect,
    Status,
    Consume,
}

#[derive(Deserialize)]
pub struct EndpointInput {
    pub name: Option<String>,
}

#[derive(Deserialize, Default)]
pub struct ConsumeArgs {
    pub timeout_ms: Option<u64>,
    pub max_messages: Option<usize>,
}

/// Serialize `data` as pretty JSON and return a successful CallToolResult.
/// Accepts a `serde_json::Value` directly to avoid double-serialization.
pub fn success_value(data: serde_json::Value) -> rmcp::model::CallToolResult {
    let content_str = serde_json::to_string_pretty(&data).unwrap_or_default();
    rmcp::model::CallToolResult::success(vec![rmcp::model::Content::text(content_str)])
}

/// Convenience wrapper for plain string results.
pub fn success_str(msg: impl Into<String>) -> rmcp::model::CallToolResult {
    rmcp::model::CallToolResult::success(vec![rmcp::model::Content::text(msg.into())])
}

pub fn to_rmcp_tool(tool: &McpTool) -> Tool {
    let schema = match tool.action {
        McpToolAction::Publish => serde_json::json!({
            "type": "object",
            "properties": { "message": { "$ref": "#/$defs/message" } },
            "required": ["message"],
            "$defs": {
                "message": {
                    "type": "object",
                    "properties": {
                        "payload": tool.schema.clone().unwrap_or_else(|| serde_json::json!({
                            "description": "The message content (string, JSON object, or any JSON value)"
                        })),
                        "metadata": { "type": "object", "additionalProperties": { "type": "string" } },
                        "message_id": { "description": "The message ID (string, integer, or MongoDB OID object)" }
                    },
                    "required": ["payload"]
                }
            }
        }),
        McpToolAction::Introspect => serde_json::json!({
            "type": "object",
            "properties": {}
        }),
        McpToolAction::Status => serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            }
        }),
        McpToolAction::Consume => serde_json::json!({
            "type": "object",
            "properties": {
                "timeout_ms": { "type": "integer" },
                "max_messages": { "type": "integer" }
            }
        }),
    };

    let mut rmcp_tool = Tool::default();
    rmcp_tool.name = tool.name.clone().into();
    rmcp_tool.description = Some(tool.description.clone().into());
    rmcp_tool.input_schema = serde_json::from_value(schema).unwrap_or_default();
    rmcp_tool
}
