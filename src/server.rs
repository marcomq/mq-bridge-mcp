use mq_bridge::CanonicalMessage;
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, Service,
    model::{
        Annotated, CallToolRequestParams, CallToolResult, ClientRequest, EmptyResult,
        ListResourcesResult, ListToolsResult, PaginatedRequestParams, RawResource,
        ReadResourceRequestParams, ReadResourceResult, ResourceContents,
        ResourceUpdatedNotification, ResourceUpdatedNotificationParam, ServerCapabilities,
        ServerInfo, ServerResult,
    },
    service::{NotificationContext, RequestContext, ServiceExt},
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::configuration::{ConsumerConfig, McpAppConfig, McpConfig, McpTransport, WatcherMode};
use crate::consumer_registry;
use crate::publisher_registry;
use crate::tools::{self, ConnectorRef, ConsumeArgs, EndpointInput, McpTool, McpToolAction};

fn mcp_invalid(msg: String) -> McpError {
    McpError::invalid_params(msg, None)
}

fn mcp_internal(msg: String) -> McpError {
    McpError::internal_error(msg, None)
}

/// Convert an endpoint config name to a tool name suffix (replaces `-` with `_`).
fn to_tool_suffix(name: &str) -> String {
    name.replace('-', "_")
}

pub struct MqBridgeMcpServer {
    /// Static tools (list_publishers, list_consumers, get_status) plus all
    /// dynamic publish_to_* and consume_from_* tools, built once at startup.
    tools: Arc<Vec<McpTool>>,
    server_name: Arc<str>,
    consumers: Arc<HashMap<String, ConsumerConfig>>,
    subscription_manager: Arc<SubscriptionManager>,
}

impl Clone for MqBridgeMcpServer {
    fn clone(&self) -> Self {
        Self {
            tools: self.tools.clone(),
            server_name: self.server_name.clone(),
            consumers: self.consumers.clone(),
            subscription_manager: self.subscription_manager.clone(),
        }
    }
}

struct SubscriptionManager {
    subscribers: RwLock<HashMap<String, Vec<rmcp::service::Peer<RoleServer>>>>,
    active_watchers: RwLock<HashSet<String>>,
    consumers: Arc<HashMap<String, ConsumerConfig>>,
}

impl SubscriptionManager {
    fn new(consumers: Arc<HashMap<String, ConsumerConfig>>) -> Self {
        Self {
            subscribers: RwLock::new(HashMap::new()),
            active_watchers: RwLock::new(HashSet::new()),
            consumers,
        }
    }

    async fn notify(&self, uri: &str) {
        let mut subs = self.subscribers.write().await;
        if let Some(peers) = subs.get_mut(uri) {
            let notification = ResourceUpdatedNotification::new(ResourceUpdatedNotificationParam {
                uri: uri.to_string(),
            });

            let mut active_peers = Vec::new();
            for peer in peers.drain(..) {
                if !peer.is_transport_closed() {
                    let p = peer.clone();
                    let n = notification.clone();
                    tokio::spawn(async move {
                        if let Err(e) = p.send_notification(n.into()).await {
                            warn!("Failed to notify peer: {}", e);
                        }
                    });
                    active_peers.push(peer);
                }
            }
            *peers = active_peers;
        }
    }

    async fn ensure_watcher(self: &Arc<Self>, uri: String) {
        let mut watchers = self.active_watchers.write().await;
        if watchers.contains(&uri) {
            return;
        }

        if let Some(consumer_name) = uri.strip_prefix("mq://")
            && let Some(def) = self.consumers.get(consumer_name) {
                if matches!(def.watcher_mode, WatcherMode::None) {
                    return;
                }

                watchers.insert(uri.clone());
                let manager = self.clone();
                let uri_clone = uri.clone();
                let consumer_name = consumer_name.to_string();
                let endpoint = def.endpoint.clone();
                let mode = def.watcher_mode.clone();
                let peek_delay = def.peek_delay_ms;

                tokio::spawn(async move {
                    info!("Starting watcher for {}", uri_clone);
                    let mut consumer = match endpoint.create_consumer(&consumer_name).await {
                        Ok(c) => c,
                        Err(e) => {
                            error!("Failed to create watcher consumer for {}: {}", uri_clone, e);
                            manager.active_watchers.write().await.remove(&uri_clone);
                            return;
                        }
                    };

                    loop {
                        match consumer.receive().await {
                            Ok(received) => {
                                manager.notify(&uri_clone).await;
                                let disposition = match mode {
                                    WatcherMode::Consume => {
                                        mq_bridge::traits::MessageDisposition::Ack
                                    }
                                    WatcherMode::Peek => {
                                        tokio::time::sleep(Duration::from_millis(peek_delay)).await;
                                        mq_bridge::traits::MessageDisposition::Nack
                                    }
                                    WatcherMode::None => {
                                        mq_bridge::traits::MessageDisposition::Nack
                                    }
                                };
                                let _ = (received.commit)(disposition).await;
                            }
                            Err(e) => {
                                error!("Watcher for {} failed: {}", uri_clone, e);
                                break;
                            }
                        }
                        let subs = manager.subscribers.read().await;
                        if let Some(list) = subs.get(&uri_clone) {
                            if list.is_empty() {
                                break;
                            }
                        } else {
                            break;
                        }
                    }
                    info!("Stopping watcher for {}", uri_clone);
                    manager.active_watchers.write().await.remove(&uri_clone);
                });
            }
    }
}

#[derive(Clone)]
struct SubscriptionMiddleware<S> {
    inner: S,
    manager: Arc<SubscriptionManager>,
}

impl<S> Service<RoleServer> for SubscriptionMiddleware<S>
where
    S: Service<RoleServer> + Send + Sync + 'static,
{
    async fn handle_request(
        &self,
        request: ClientRequest,
        ctx: RequestContext<RoleServer>,
    ) -> Result<ServerResult, McpError> {
        match request {
            ClientRequest::SubscribeRequest(req) => {
                info!("Subscribing to resource: {}", req.params.uri);
                {
                    let mut subs = self.manager.subscribers.write().await;
                    subs.entry(req.params.uri.clone())
                        .or_default()
                        .push(ctx.peer.clone());
                }
                self.manager.ensure_watcher(req.params.uri.clone()).await;
                Ok(ServerResult::EmptyResult(EmptyResult {}))
            }
            ClientRequest::UnsubscribeRequest(req) => {
                info!("Unsubscribing from resource: {}", req.params.uri);
                Ok(ServerResult::EmptyResult(EmptyResult {}))
            }
            _ => self.inner.handle_request(request, ctx).await,
        }
    }

    async fn handle_notification(
        &self,
        notification: <RoleServer as rmcp::service::ServiceRole>::PeerNot,
        ctx: NotificationContext<RoleServer>,
    ) -> Result<(), McpError> {
        self.inner.handle_notification(notification, ctx).await
    }

    fn get_info(&self) -> ServerInfo {
        self.inner.get_info()
    }
}

impl MqBridgeMcpServer {
    pub fn new(config: &McpAppConfig, server_name: impl Into<String>) -> Self {
        let consumers = Arc::new(config.consumers.clone());
        Self {
            tools: Arc::new(Self::build_tools(config)),
            server_name: server_name.into().into(),
            consumers: consumers.clone(),
            subscription_manager: Arc::new(SubscriptionManager::new(consumers)),
        }
    }

    fn build_tools(config: &McpAppConfig) -> Vec<McpTool> {
        let mut tool_list = vec![
            McpTool {
                name: "list_publishers".into(),
                description: "Lists all available publishers that can be used to send messages."
                    .into(),
                action: McpToolAction::Introspect,
                connector: ConnectorRef {
                    endpoint_name: None,
                    connector_type: "internal".into(),
                },
                schema: None,
            },
            McpTool {
                name: "list_consumers".into(),
                description: "Lists all available consumers that can be used to receive messages."
                    .into(),
                action: McpToolAction::Introspect,
                connector: ConnectorRef {
                    endpoint_name: None,
                    connector_type: "internal_consumer".into(),
                },
                schema: None,
            },
            McpTool {
                name: "get_status".into(),
                description: "Returns the status of registered publishers and consumers.".into(),
                action: McpToolAction::Status,
                connector: ConnectorRef {
                    endpoint_name: None,
                    connector_type: "internal".into(),
                },
                schema: None,
            },
        ];

        for name in publisher_registry::list_publishers() {
            if let Some(pub_def) = publisher_registry::get_publisher(&name) {
                let tool_name = format!("publish_to_{}", to_tool_suffix(&name));
                let mut description = pub_def.description.clone();
                if pub_def.destructive {
                    description.push_str(" (Destructive)");
                }
                if pub_def.open_world {
                    description.push_str(" (Open World)");
                }
                if pub_def.idempotent {
                    description.push_str(" (Idempotent)");
                }

                let schemas = config
                    .publishers
                    .get(&name)
                    .map(|p| p.payload_schemas.clone())
                    .unwrap_or_default();

                if schemas.is_empty() {
                    tool_list.push(McpTool {
                        name: tool_name,
                        description,
                        action: McpToolAction::Publish,
                        connector: ConnectorRef {
                            connector_type: pub_def.endpoint_type.clone(),
                            endpoint_name: Some(name.clone()),
                        },
                        schema: None,
                    });
                } else {
                    for schema in schemas {
                        tool_list.push(McpTool {
                            name: format!("{}_{}", tool_name, schema.name),
                            description: schema.description,
                            action: McpToolAction::Publish,
                            connector: ConnectorRef {
                                connector_type: pub_def.endpoint_type.clone(),
                                endpoint_name: Some(name.clone()),
                            },
                            schema: Some(schema.schema),
                        });
                    }
                }
            }
        }

        for (name, def) in config.consumers.iter() {
            mq_bridge::endpoints::check_consumer(name, &def.endpoint, None)
                .expect("Invalid consumer config: {name}");
            let tool_name = format!("consume_from_{}", to_tool_suffix(name));
            tool_list.push(McpTool {
                name: tool_name,
                description: def.description.clone(),
                action: McpToolAction::Consume,
                connector: ConnectorRef {
                    connector_type: def.endpoint.endpoint_type.name().to_string(),
                    endpoint_name: Some(name.clone()),
                },
                schema: None,
            });
        }

        tool_list.sort_by(|a, b| a.name.cmp(&b.name));
        tool_list
    }

    pub async fn start(self, mcp_config: &McpConfig) -> anyhow::Result<()> {
        let bind = &mcp_config.bind;
        match mcp_config.transport {
            McpTransport::StreamableHttp => self.start_streamable_http(bind, &mcp_config.tls).await,
            McpTransport::Stdio => self.start_stdio().await,
        }
    }

    async fn start_streamable_http(
        self,
        bind: &str,
        tls_config: &Option<mq_bridge::models::TlsConfig>,
    ) -> anyhow::Result<()> {
        use axum::serve;
        use axum::{Router, routing::get};
        use rmcp::transport::streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
        };

        let addr: std::net::SocketAddr = bind.parse()?;
        info!("MCP StreamableHTTP server starting on {}", addr);

        let me = self.clone();
        let service = StreamableHttpService::new(
            move || {
                Ok(SubscriptionMiddleware {
                    inner: me.clone(),
                    manager: me.subscription_manager.clone(),
                })
            },
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default(),
        );

        let router = Router::new()
            .route(
                "/",
                get(|| async { "MCP endpoint is running. Please use a compatible client." }),
            )
            .nest_service("/mcp", service);

        if let Some(tls) = tls_config {
            info!("TLS enabled for MCP server");
            let tls_cfg = axum_server::tls_rustls::RustlsConfig::from_pem_file(
                tls.cert_file
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("TLS cert_file required"))?,
                tls.key_file
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("TLS key_file required"))?,
            )
            .await?;

            axum_server::bind_rustls(addr, tls_cfg)
                .serve(router.into_make_service())
                .await?;
        } else {
            let listener = tokio::net::TcpListener::bind(addr).await?;
            serve(listener, router).await?;
        }

        Ok(())
    }

    async fn start_stdio(self) -> anyhow::Result<()> {
        use rmcp::transport::stdio;

        info!("MCP stdio server starting");
        let service = SubscriptionMiddleware {
            manager: self.subscription_manager.clone(),
            inner: self,
        }
        .serve(stdio())
        .await?;
        service.waiting().await?;
        Ok(())
    }

    async fn handle_tool_call(
        &self,
        tool: &McpTool,
        args: Option<serde_json::Value>,
    ) -> Result<CallToolResult, McpError> {
        let args = args.unwrap_or(serde_json::Value::Object(Default::default()));

        match &tool.action {
            McpToolAction::Publish => {
                let m_val = args
                    .get("message")
                    .ok_or_else(|| mcp_invalid("Missing 'message' argument".into()))?;

                // If the input matches our schema { payload: ..., metadata: ... }, use it.
                // Otherwise, treat the whole object as payload (legacy behavior).
                let canonical_message = if let Some(obj) = m_val.as_object() {
                    if let Some(payload_val) = obj.get("payload") {
                        let mut m = if let Some(s) = payload_val.as_str() {
                            CanonicalMessage::from(s)
                        } else {
                            let bytes = serde_json::to_vec(payload_val)
                                .map_err(|e| mcp_internal(e.to_string()))?;
                            CanonicalMessage::new(bytes, None)
                        };

                        if let Some(id_val) = obj.get("message_id") {
                            // Use CanonicalMessage's own parser logic for ID
                            if let Ok(tm) = CanonicalMessage::from_json(
                                serde_json::json!({ "message_id": id_val }),
                            ) {
                                m.message_id = tm.message_id;
                            }
                        }
                        if let Some(meta) = obj.get("metadata").and_then(|v| v.as_object()) {
                            for (k, v) in meta {
                                if let Some(s) = v.as_str() {
                                    m.metadata.insert(k.clone(), s.to_string());
                                }
                            }
                        }
                        m
                    } else {
                        CanonicalMessage::from_json(m_val.clone())
                            .map_err(|e| mcp_invalid(e.to_string()))?
                    }
                } else {
                    CanonicalMessage::from_json(m_val.clone())
                        .map_err(|e| mcp_invalid(e.to_string()))?
                };

                let endpoint_name = tool.connector.endpoint_name.as_deref().unwrap_or_default();
                let Some(pub_def) = publisher_registry::get_publisher(endpoint_name) else {
                    return Err(mcp_invalid(format!(
                        "Publisher '{}' not found",
                        endpoint_name
                    )));
                };

                let publisher = pub_def.publisher.clone();
                match publisher.send(canonical_message).await {
                    Ok(_) => Ok(tools::success_str("Message published successfully")),
                    Err(e) => Err(mcp_internal(e.to_string())),
                }
            }

            McpToolAction::Introspect => {
                if tool.name == "list_consumers" {
                    let mut consumer_list: Vec<serde_json::Value> = self
                        .consumers
                        .iter()
                        .map(|(name, def)| {
                            serde_json::json!({
                                "name": format!("consume_from_{}", to_tool_suffix(name)),
                                "description": def.description,
                                "type": def.endpoint.endpoint_type.name(),
                                "read_only": def.read_only,
                                "open_world": def.open_world,
                                "idempotent": def.idempotent,
                            })
                        })
                        .collect();
                    consumer_list.sort_by(|a, b| {
                        a["name"]
                            .as_str()
                            .unwrap_or("")
                            .cmp(b["name"].as_str().unwrap_or(""))
                    });
                    // Pass Value directly — no double-serialization.
                    return Ok(tools::success_value(serde_json::Value::Array(
                        consumer_list,
                    )));
                }

                // Default: list_publishers
                let mut endpoint_list: Vec<serde_json::Value> = Vec::new();
                for name in publisher_registry::list_publishers() {
                    if let Some(pub_def) = publisher_registry::get_publisher(&name) {
                        let mut description = pub_def.description.clone();
                        if pub_def.destructive {
                            description.push_str(" (Destructive)");
                        }
                        if pub_def.open_world {
                            description.push_str(" (Open World)");
                        }
                        if pub_def.idempotent {
                            description.push_str(" (Idempotent)");
                        }
                        endpoint_list.push(serde_json::json!({
                            "name": format!("publish_to_{}", to_tool_suffix(&name)),
                            "description": description,
                            "type": pub_def.endpoint_type,
                            "destructive": pub_def.destructive,
                            "open_world": pub_def.open_world,
                            "idempotent": pub_def.idempotent,
                        }));
                    }
                }
                endpoint_list.sort_by(|a, b| {
                    a["name"]
                        .as_str()
                        .unwrap_or("")
                        .cmp(b["name"].as_str().unwrap_or(""))
                });

                Ok(tools::success_value(serde_json::Value::Array(
                    endpoint_list,
                )))
            }

            McpToolAction::Consume => {
                let endpoint_name = tool.connector.endpoint_name.as_deref().unwrap_or_default();
                let Some(consumer_def) = self.consumers.get(endpoint_name) else {
                    return Err(mcp_invalid(format!(
                        "Consumer '{}' not found",
                        endpoint_name
                    )));
                };

                let consume_args: ConsumeArgs = serde_json::from_value(args).unwrap_or_default();
                let timeout = Duration::from_millis(consume_args.timeout_ms.unwrap_or(5000));
                let max_messages = consume_args.max_messages.unwrap_or(10);

                // Reuse pooled consumer if available; fall back to creating one.
                let consumer_arc = match consumer_registry::get_consumer(endpoint_name) {
                    Some(c) => c,
                    None => {
                        let c = consumer_def
                            .endpoint
                            .create_consumer(endpoint_name)
                            .await
                            .map_err(|e| {
                                mcp_internal(format!(
                                    "Failed to create consumer for '{}': {}",
                                    endpoint_name, e
                                ))
                            })?;
                        consumer_registry::register_consumer(endpoint_name, c)
                    }
                };

                let mut consumer = consumer_arc.lock().await;
                match tokio::time::timeout(timeout, consumer.receive_batch(max_messages)).await {
                    Ok(Ok(batch)) => {
                        let msgs: Vec<serde_json::Value> = batch
                            .messages
                            .iter()
                            .map(|m| {
                                // Prefer parsed JSON; fall back to plain string.
                                serde_json::from_slice(&m.payload).unwrap_or_else(|_| {
                                    serde_json::Value::String(m.get_payload_str().to_string())
                                })
                            })
                            .collect();

                        let dispositions =
                            vec![mq_bridge::traits::MessageDisposition::Ack; batch.messages.len()];
                        (batch.commit)(dispositions).await.ok();

                        Ok(tools::success_value(serde_json::Value::Array(msgs)))
                    }
                    Ok(Err(e)) => Err(mcp_internal(format!("Error consuming batch: {}", e))),
                    Err(_) => Ok(tools::success_str(format!(
                        "Timed out after {}ms with no messages",
                        timeout.as_millis()
                    ))),
                }
            }

            McpToolAction::Status => {
                let input: EndpointInput =
                    serde_json::from_value(args).unwrap_or(EndpointInput { name: None });

                let publishers = publisher_registry::list_publishers();
                let consumers: Vec<String> = self.consumers.keys().cloned().collect();

                let status = match input.name.as_deref() {
                    Some(name) => {
                        let is_publisher = publishers.contains(&name.to_string());
                        let is_consumer = self.consumers.contains_key(name);
                        serde_json::json!({
                            "name": name,
                            "publisher": is_publisher,
                            "consumer": is_consumer,
                        })
                    }
                    None => serde_json::json!({
                        "publishers": publishers,
                        "consumers": consumers,
                    }),
                };

                Ok(tools::success_value(status))
            }
        }
    }
}

impl ServerHandler for MqBridgeMcpServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_resources()
            .build();

        if let Some(resources) = &mut info.capabilities.resources {
            resources.subscribe = Some(true);
        }

        info.instructions = Some(
            "mq-bridge MCP server. Use list_publishers / list_consumers to discover available \
             endpoints, then use publish_to_<name> or consume_from_<name> tools."
                .into(),
        );
        info.server_info.name = self.server_name.as_ref().into();
        info.server_info.version = env!("CARGO_PKG_VERSION").into();
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<rmcp::RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        // Tool list is fully built at startup — just convert and return.
        Ok(ListToolsResult {
            tools: self.tools.iter().map(tools::to_rmcp_tool).collect(),
            next_cursor: None,
            meta: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _ctx: RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        info!("MCP tool call: {}", request.name);

        let arguments = request.arguments.map(serde_json::Value::Object);

        // Look up the tool by name directly from the pre-built list.
        if let Some(tool) = self.tools.iter().find(|t| t.name == request.name) {
            return self.handle_tool_call(tool, arguments).await;
        }

        Err(mcp_invalid(format!("Unknown tool: {}", request.name)))
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<rmcp::RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let mut resources = Vec::new();
        for (name, def) in self.consumers.iter() {
            resources.push(Annotated {
                raw: RawResource::new(format!("mq://{}", name), name.clone())
                    .with_description(def.description.clone())
                    .with_mime_type("application/json"),
                annotations: None,
            });
        }
        Ok(ListResourcesResult {
            resources,
            next_cursor: None,
            meta: None,
        })
    }

    /// Returns non-destructive metadata about the endpoint.
    /// Does NOT consume or ack any messages — use `consume_from_<name>` for that.
    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _ctx: RequestContext<rmcp::RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        let uri_str = request.uri.as_str();
        let consumer_name = uri_str
            .strip_prefix("mq://")
            .ok_or_else(|| mcp_invalid(format!("Invalid resource URI: {}", uri_str)))?;

        let Some(consumer_def) = self.consumers.get(consumer_name) else {
            return Err(mcp_invalid(format!(
                "Consumer '{}' not found",
                consumer_name
            )));
        };

        let metadata = serde_json::json!({
            "name": consumer_name,
            "description": consumer_def.description,
            "broker_type": consumer_def.endpoint.endpoint_type.name(),
            "uri": uri_str,
            "read_only": consumer_def.read_only,
            "open_world": consumer_def.open_world,
            "idempotent": consumer_def.idempotent,
            "watcher_mode": consumer_def.watcher_mode,
            "peek_delay_ms": consumer_def.peek_delay_ms,
            "hint": format!(
                "Use the 'consume_from_{}' tool to receive messages from this endpoint.",
                to_tool_suffix(consumer_name)
            ),
        });

        Ok(ReadResourceResult::new(vec![ResourceContents::text(
            serde_json::to_string_pretty(&metadata).unwrap_or_default(),
            request.uri,
        )]))
    }
}
