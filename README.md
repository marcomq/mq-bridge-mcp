# mq-bridge-mcp: Messaging Infrastructure for AI


![Linux](https://img.shields.io/badge/Linux-supported-green?logo=linux)
![Windows](https://img.shields.io/badge/Windows-supported-green?logo=windows)
![macOS](https://img.shields.io/badge/macOS-supported-green?logo=apple)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

`mq-bridge-mcp` connects your AI assistants (like **Claude Desktop**) to your messaging infrastructure using the **Model Context Protocol (MCP)**.

It acts as a bridge, exposing message brokers, databases, and APIs as **Tools** and **Resources** that LLMs can natively understand and interact with.

## Features

- **Publishers (Tools)**: Allow the AI to send messages to systems (e.g., "Send an order to Kafka", "Post to Slack").
- **Consumers (Resources)**: Allow the AI to read data from systems (e.g., "Read the last 10 logs from the file", "Get messages from SQS").
- **Supported Protocols**: Kafka, AMQP (RabbitMQ), NATS, AWS SQS/SNS, MQTT, MongoDB, SQLx (Postgres, MySQL, MSSQL, SQLite), HTTP, and more.

## Getting Started

### 1. Configuration

Create a configuration file `config.yml`. This defines what the AI can see and do.

```yaml
mcp:
  enabled: true
  transport: "stdio" # Use "stdio" for desktop apps, "streamable_http" for Docker/remote

publishers:
  notify_slack:
    description: "Sends a notification to the #ops Slack channel."
    http:
      url: "https://hooks.slack.com/services/XXX/YYY"

  produce_kafka:
    description: "Publishes a command to the 'ai-commands' topic."
    kafka:
      url: "kafka:9092"
      topic: "ai-commands"

consumers:
  app_logs:
    description: "Live application logs."
    file:
      path: "/var/log/app.log"
      mode: "subscribe"
```

## Use Cases
- Incident Response
Configure a publisher using the http endpoint to send messages to Slack or Microsoft Teams webhooks. This allows the AI to draft and send incident reports directly from the chat context.

- Log Debugging
Configure a consumer using the file endpoint (in subscribe mode) to tail application logs. The AI can read the latest log lines to help diagnose errors without needing direct shell access.

- Event Bus Management
Connect to Kafka, NATS, or AMQP to inject messages that trigger a remote action.

## Usage

### Claude Desktop (Stdio Mode)

To use the MCP server with Claude Desktop, configure it to run the `mq-bridge-mcp` binary in `stdio` mode.

```json
// claude_desktop_config.json
{
  "mcpServers": {
    "mq-bridge": {
      "command": "/path/to/mq-bridge-mcp",
      "args": ["--config", "/path/to/mcp-config.yml"]
    }
  }
}
```

### Analyze locally
```
cargo build
npx @modelcontextprotocol/inspector target/debug/mq-bridge-mcp --config config/stdio_file_file.yml

```