//  mq-bridge-mcp: An MCP server for mq-bridge endpoints.
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge-mcp

mod configuration;
mod consumer_registry;
mod publisher_registry;
mod server;
mod tools;

use anyhow::Context;
use clap::Parser;
use std::time::Duration;
use tracing::{error, info, warn};

use crate::configuration::{McpAppConfig, McpTransport, load_mcp_app_config};
use crate::server::MqBridgeMcpServer;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    /// Path to the configuration file
    #[arg(short, long, default_value = "config.yml")]
    pub config: String,

    /// Generate the JSON schema for the configuration
    #[arg(long)]
    pub schema: Option<String>,

    /// Override transport (stdio or streamable_http)
    #[arg(long)]
    pub transport: Option<String>,

    /// Override port (for streamable_http)
    #[arg(long)]
    pub port: Option<u16>,
}

fn apply_overrides(config: &mut McpAppConfig, args: &Cli) {
    if let Some(transport) = &args.transport {
        config.mcp.transport = match transport.as_str() {
            "streamable_http" => McpTransport::StreamableHttp,
            "stdio" => McpTransport::Stdio,
            _ => {
                warn!("Unknown transport '{}', defaulting to Stdio", transport);
                McpTransport::Stdio
            }
        };
    }

    if let Some(port) = args.port {
        let current_bind = if config.mcp.bind.is_empty() {
            "0.0.0.0:3000".to_string()
        } else {
            config.mcp.bind.clone()
        };

        if let Ok(mut addr) = current_bind.parse::<std::net::SocketAddr>() {
            addr.set_port(port);
            config.mcp.bind = addr.to_string();
        } else {
            config.mcp.bind = format!("0.0.0.0:{}", port);
        }
    }

    if config.mcp.bind.is_empty() {
        config.mcp.bind = "0.0.0.0:3000".to_string();
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    let args = Cli::parse();

    if let Some(schema_path) = args.schema {
        let schema = schemars::schema_for!(McpAppConfig);
        let schema_json =
            serde_json::to_string_pretty(&schema).context("Failed to serialize schema")?;

        if schema_path == "-" {
            println!("{}", schema_json);
        } else {
            let path = std::path::Path::new(&schema_path);
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
                && !parent.exists()
            {
                std::fs::create_dir_all(parent)
                    .context("Failed to create parent directory for schema")?;
            }
            std::fs::write(path, schema_json).context("Failed to write schema file")?;
        }
        return Ok(());
    }

    let mut config = load_mcp_app_config(&args.config)?;

    // Apply CLI overrides
    apply_overrides(&mut config, &args);

    // Register publishers from config
    for (name, publisher_conf) in &config.publishers {
        let mut route = mq_bridge::Route::new(
            mq_bridge::models::Endpoint {
                endpoint_type: mq_bridge::models::EndpointType::Null,
                ..Default::default()
            },
            publisher_conf.endpoint.clone(),
        );
        route.options.description = publisher_conf.description.clone();

        let mut attempts = 0;
        const MAX_ATTEMPTS: u32 = 10;
        if let Err(e) = mq_bridge::endpoints::check_publisher(name, &route.output, None) {
            error!("Configuration check failed for publisher '{}': {}. It may not function correctly.", name, e);
        }

        while attempts < MAX_ATTEMPTS {
            attempts += 1;
            match route.create_publisher().await {
                Ok(publisher) => {
                    publisher_registry::register_publisher(
                        name,
                        crate::publisher_registry::PublisherDefinition {
                            publisher,
                            description: route.options.description.clone(),
                            endpoint_type: route.output.endpoint_type.name().to_string(),
                            destructive: publisher_conf.destructive,
                            open_world: publisher_conf.open_world,
                            idempotent: publisher_conf.idempotent,
                        },
                    );
                    info!("Registered publisher '{}'", name);
                    break;
                }
                Err(e) => {
                    if attempts == MAX_ATTEMPTS {
                        warn!(
                            "Failed to register publisher '{}' after {} attempts: {}. It will be unavailable.",
                            name, attempts, e
                        );
                    } else {
                        warn!(
                            "Failed to register publisher '{}': {}. Retrying (attempt {}/{})...",
                            name, e, attempts, MAX_ATTEMPTS
                        );
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                }
            }
        }
    }

    let server = MqBridgeMcpServer::new(&config, "mq-bridge-mcp");
    server.start(&config.mcp).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cli_parsing() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn test_apply_overrides() {
        let mut config = McpAppConfig::default();
        let args = Cli {
            config: "config.yml".into(),
            schema: None,
            transport: Some("streamable_http".into()),
            port: Some(9090),
        };
        apply_overrides(&mut config, &args);
        assert!(matches!(config.mcp.transport, McpTransport::StreamableHttp));
        assert!(config.mcp.bind.ends_with(":9090"));
    }
}
