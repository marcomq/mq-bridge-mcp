use config::Config;
use mq_bridge::models::{Endpoint, SecretExtractor, TlsConfig};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// API Key authentication details.
#[derive(Debug, serde::Deserialize, serde::Serialize, JsonSchema, Clone, Default, PartialEq)]
pub struct ApiKeyAuth {
    /// The HTTP header to check for the API key. Defaults to "Authorization".
    #[serde(default)]
    pub header: String,
    /// The secret API key or token.
    #[serde(default)]
    pub key: String,
}

/// Authentication methods for the MCP server.
#[derive(Debug, serde::Deserialize, serde::Serialize, JsonSchema, Clone, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum McpAuth {
    #[default]
    None,
    /// API Key authentication.
    ApiKey(ApiKeyAuth),
}

/// Transport protocol for the MCP server.
#[derive(Debug, serde::Deserialize, serde::Serialize, JsonSchema, Clone, Default)]
#[serde(rename_all = "snake_case")]
pub enum McpTransport {
    /// Use a streamable HTTP transport, which supports Server-Sent Events (SSE).
    StreamableHttp,
    /// Use standard input/output for communication.
    #[default]
    Stdio,
}

/// Configuration for the Marco's Control Plane (MCP) server.
/// MCP provides a remote API for interacting with and managing the bridge.
#[derive(Debug, serde::Deserialize, serde::Serialize, JsonSchema, Clone, Default)]
pub struct McpConfig {
    /// If true, the MCP server is enabled.
    #[serde(default)]
    pub enabled: bool,
    /// The transport protocol to use for the server.
    #[serde(default)]
    pub transport: McpTransport,
    /// The address to bind the server to (e.g., "0.0.0.0:9092").
    #[serde(default)]
    pub bind: String,
    /// Authentication settings for the server. If not present, no auth is used.
    #[serde(default)]
    pub auth: McpAuth,
    /// Optional timeout for consuming messages (in milliseconds).
    #[serde(default)]
    pub consume_timeout_ms: u64,
    /// Optional TLS configuration for the server.
    #[serde(default)]
    pub tls: Option<TlsConfig>,
}

impl SecretExtractor for ApiKeyAuth {
    fn extract_secrets(&mut self, prefix: &str, secrets: &mut HashMap<String, String>) {
        if !self.key.is_empty() && !self.key.starts_with("${") {
            let key_name = format!("{}KEY", prefix);
            secrets.insert(key_name.clone(), self.key.clone());
            self.key = format!("${{{}}}", key_name);
        }
    }
}

impl SecretExtractor for McpAuth {
    fn extract_secrets(&mut self, prefix: &str, secrets: &mut HashMap<String, String>) {
        match self {
            McpAuth::ApiKey(api_key_auth) => {
                api_key_auth.extract_secrets(&format!("{}API_KEY__", prefix), secrets);
            }
            McpAuth::None => {}
        }
    }
}

#[derive(Debug, Deserialize, Serialize, JsonSchema, Clone)]
pub struct PayloadSchema {
    pub name: String,
    pub description: String,
    pub schema: serde_json::Value,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema, Clone)]
pub struct PublisherConfig {
    #[serde(flatten)]
    pub endpoint: Endpoint,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub destructive: bool,
    #[serde(default)]
    pub open_world: bool,
    #[serde(default)]
    pub idempotent: bool,
    #[serde(default)]
    pub payload_schemas: Vec<PayloadSchema>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema, Clone, Default)]
#[serde(rename_all = "snake_case")]
pub enum WatcherMode {
    /// Consumes and Acks messages. WARNING: This is destructive! The message will be lost (Acked) merely to trigger a notification.
    Consume,
    /// Consumes and Nacks messages (requeue). Attempts to peek.
    /// Note: This may cause busy loops if peek_delay_ms is low.
    Peek,
    /// No automatic watching. Notifications will not be generated.
    #[default]
    None,
}

pub fn default_peek_delay() -> u64 {
    1000
}

#[derive(Debug, Deserialize, Serialize, JsonSchema, Clone)]
pub struct ConsumerConfig {
    #[serde(flatten)]
    pub endpoint: Endpoint,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub open_world: bool,
    #[serde(default)]
    pub idempotent: bool,
    #[serde(default)]
    pub watcher_mode: WatcherMode,
    #[serde(default = "default_peek_delay")]
    pub peek_delay_ms: u64,
}

fn default_log_level() -> String {
    "info".to_string()
}

#[derive(Debug, Deserialize, Serialize, JsonSchema, Clone, Default)]
pub struct McpAppConfig {
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default)]
    pub logger: String,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub publishers: HashMap<String, PublisherConfig>,
    #[serde(default)]
    pub consumers: HashMap<String, ConsumerConfig>,
}

pub fn load_mcp_app_config(config_path: &str) -> anyhow::Result<McpAppConfig> {
    let builder = Config::builder()
        .add_source(config::File::with_name(config_path).required(true))
        .add_source(config::Environment::with_prefix("MCP_SERVER").separator("__"));
    let settings = builder.build()?;
    let config: McpAppConfig = settings.try_deserialize()?;
    Ok(config)
}
