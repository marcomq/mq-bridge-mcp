use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

static DOCKER_LOCK: Mutex<()> = Mutex::new(());

/// A RAII wrapper around docker-compose to ensure environments are torn down.
struct DockerEnvironment {
    compose_file: String,
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl DockerEnvironment {
    fn new(service: &str) -> Self {
        let _guard = match DOCKER_LOCK.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let root = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
        let compose_file = Path::new(&root)
            .join("tests")
            .join("docker-compose")
            .join(format!("{}.yml", service))
            .to_string_lossy()
            .into_owned();

        if !Path::new(&compose_file).exists() {
            eprintln!(
                "Warning: {} not found. Ensure it exists in the project root.",
                compose_file
            );
        }

        // Ensure clean state before starting
        let _ = Command::new("docker")
            .args(&[
                "compose",
                "-f",
                &compose_file,
                "down",
                "-v",
                "--remove-orphans",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        println!("Starting {} environment...", service);
        let status = Command::new("docker")
            .args(&["compose", "-f", &compose_file, "up", "-d"])
            .status()
            .expect("Failed to execute docker compose");

        if !status.success() {
            panic!("Failed to start docker compose environment for {}", service);
        }

        // Wait for services to be ready.
        wait_for_healthy(&compose_file);

        Self {
            compose_file,
            _guard,
        }
    }
}

impl Drop for DockerEnvironment {
    fn drop(&mut self) {
        println!("Tearing down {} environment...", self.compose_file);
        let _ = Command::new("docker")
            .args(&[
                "compose",
                "-f",
                &self.compose_file,
                "down",
                "-v",
                "--remove-orphans",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn wait_for_healthy(compose_file: &str) {
    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(30);
    while start.elapsed() < timeout {
        let output = Command::new("docker")
            .args(&["compose", "-f", compose_file, "ps"])
            .output()
            .expect("Failed to execute docker compose ps");

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if stdout.contains("(healthy)")
                && !stdout.contains("(health: starting)")
                && !stdout.contains("(unhealthy)")
            {
                return;
            }
        } else {
            // Docker compose command failed, retry briefly
            return;
        }
        thread::sleep(Duration::from_millis(1000));
    }
    panic!(
        "Services in {} did not become healthy in time",
        compose_file
    );
}

// Note: Run these tests with `cargo test -- --test-threads=1 --ignored` to ensure
// sequential execution (avoiding port conflicts) and to include these ignored tests.

fn run_integration_test(
    config_yaml: &str,
    tool_name: &str,
    consume_tool: Option<&str>,
    warmup: bool,
) {
    // Write config to a temporary file
    let config_path = std::env::temp_dir().join(format!("mcp_test_{}.yml", tool_name));
    std::fs::write(&config_path, config_yaml).expect("Failed to write test config");

    let bin_path_env = std::env::var("MQ_BRIDGE_BINARY").ok();
    let bin_path_default = env!("CARGO_BIN_EXE_mq-bridge-mcp");
    let bin_path = bin_path_env.as_deref().unwrap_or(bin_path_default);

    let mut child = Command::new(bin_path)
        .arg("--config")
        .arg(&config_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("Failed to spawn mq-bridge-mcp");

    let stdin = child.stdin.as_mut().expect("Failed to open stdin");
    let stdout = child.stdout.as_mut().expect("Failed to open stdout");
    let mut reader = BufReader::new(stdout);

    // 1. Send Initialize
    let init_req = r#"{"jsonrpc":"2.0","method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}},"id":1}"#;
    stdin
        .write_all(format!("{}\n", init_req).as_bytes())
        .expect("Failed to write init");

    let mut line = String::new();
    reader
        .read_line(&mut line)
        .expect("Failed to read init response");
    assert!(line.contains("result"), "Init failed: {}", line);

    // 2. Send Initialized Notification
    let initialized_notif = r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#;
    stdin
        .write_all(format!("{}\n", initialized_notif).as_bytes())
        .expect("Failed to write initialized");

    // Optional Warmup: Initialize consumer to ensure subscription/session exists before publishing
    if warmup {
        if let Some(consume_name) = consume_tool {
            let warmup_req = format!(
                r#"{{"jsonrpc":"2.0","method":"tools/call","params":{{"name":"{}","arguments":{{"timeout_ms":1000,"max_messages":1}}}},"id":999}}"#,
                consume_name
            );
            stdin
                .write_all(format!("{}\n", warmup_req).as_bytes())
                .expect("Failed to write warmup call");

            let mut warmup_line = String::new();
            reader
                .read_line(&mut warmup_line)
                .expect("Failed to read warmup response");
            // We ignore the result of warmup (likely a timeout)
        }
    }

    // 3. Call Publish Tool
    let call_req = format!(
        r#"{{"jsonrpc":"2.0","method":"tools/call","params":{{"name":"{}","arguments":{{"message":{{"payload":"hello from integration test"}}}}}},"id":2}}"#,
        tool_name
    );
    stdin
        .write_all(format!("{}\n", call_req).as_bytes())
        .expect("Failed to write tool call");

    line.clear();
    reader
        .read_line(&mut line)
        .expect("Failed to read tool response");

    // 4. Assert Success
    // We expect a successful result, not an error.
    if line.contains("\"error\"") {
        panic!("Tool call failed: {}", line);
    }
    assert!(
        line.contains("Message published successfully"),
        "Unexpected response: {}",
        line
    );

    if let Some(consume_name) = consume_tool {
        // thread::sleep(Duration::from_millis(30000));
        // 5. Call Consume Tool
        let consume_req = format!(
            r#"{{"jsonrpc":"2.0","method":"tools/call","params":{{"name":"{}","arguments":{{"timeout_ms":5000,"max_messages":1}}}},"id":3}}"#,
            consume_name
        );
        stdin
            .write_all(format!("{}\n", consume_req).as_bytes())
            .expect("Failed to write consume call");

        line.clear();
        reader
            .read_line(&mut line)
            .expect("Failed to read consume response");
        // thread::sleep(Duration::from_millis(100000));

        if line.contains("\"error\"") {
            panic!("Consume tool call failed: {}", line);
        }
        assert!(
            line.contains("hello from integration test"),
            "Consume response did not contain expected payload. Got: {}",
            line
        );
    }

    // Cleanup
    let _ = child.kill();
}

#[test]
fn test_kafka_integration() {
    let _env = DockerEnvironment::new("kafka");

    let config = r#"
mcp:
  transport: stdio
publishers:
  kafka_test:
    kafka:
      url: "localhost:9092"
      topic: "test_topic_mcp"
      delayed_ack: false
      producer_options: 
        - ["queue.buffering.max.ms", "50"]
        - ["acks", "1"]
        - ["compression.type", "snappy"]
    description: "Integration Test Kafka"
consumers:
  kafka_test:
    kafka:
      url: "localhost:9092"
      topic: "test_topic_mcp"
      group_id: "test_group"
      consumer_options:
        - ["auto.offset.reset", "earliest"]
    description: "Integration Test Kafka Consumer"
"#;

    run_integration_test(
        config,
        "publish_to_kafka_test",
        Some("consume_from_kafka_test"),
        false,
    );
}

#[test]
fn test_amqp_integration() {
    let _env = DockerEnvironment::new("amqp");

    let config = r#"
mcp:
  transport: stdio
publishers:
  amqp_test:
    amqp:
      url: "amqp://guest:guest@localhost:5672/%2f"
      queue: "test_queue"
    description: "Integration Test AMQP"
consumers:
  amqp_test:
    amqp:
      url: "amqp://guest:guest@localhost:5672/%2f"
      queue: "test_queue"
    description: "Integration Test AMQP Consumer"
"#;

    run_integration_test(
        config,
        "publish_to_amqp_test",
        Some("consume_from_amqp_test"),
        false,
    );
}

#[test]
fn test_postgres_integration() {
    let _env = DockerEnvironment::new("postgres");

    let config = r#"
mcp:
  transport: stdio
publishers:
  postgres_test:
    sqlx:
      url: "postgres://testuser:testpass@localhost:5432/testdb"
      table: "messages"
      auto_create_table: true
    description: "Integration Test Postgres Publisher"
consumers:
  postgres_test:
    sqlx:
      url: "postgres://testuser:testpass@localhost:5432/testdb"
      table: "messages"
      delete_after_read: true
    description: "Integration Test Postgres Consumer"
"#;

    run_integration_test(
        config,
        "publish_to_postgres_test",
        Some("consume_from_postgres_test"),
        false,
    );
}

#[test]
fn test_file_integration() {
    // No Docker needed for file test
    let temp_file =
        std::env::temp_dir().join(format!("mq_test_{}.txt", fast_uuid_v7::gen_id_str()));
    let file_path_str = temp_file.to_string_lossy().replace('\\', "/");

    let config = format!(
        r#"
mcp:
  transport: stdio
publishers:
  file_test:
    file:
      path: "{}"
      format: raw
    description: "Integration Test File"
consumers:
  file_test:
    file:
      path: "{}"
      mode: consume
    description: "Integration Test File Consumer"
"#,
        file_path_str, file_path_str
    );

    run_integration_test(
        &config,
        "publish_to_file_test",
        Some("consume_from_file_test"),
        false,
    );

    // Cleanup
    let _ = std::fs::remove_file(temp_file);
}

#[test]
fn test_http_integration() {
    // Start a simple TCP listener to act as the HTTP server
    let listener = TcpListener::bind("127.0.0.1:0").expect("Failed to bind to free port");
    let port = listener.local_addr().unwrap().port();

    // Spawn a thread to handle the incoming request
    thread::spawn(move || {
        for stream in listener.incoming() {
            if let Ok(mut stream) = stream {
                let mut buf = [0; 1024];
                let _ = stream.read(&mut buf);
                let response = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
                let _ = stream.write_all(response.as_bytes());
                return; // Handle one request and exit
            }
        }
    });

    let config = format!(
        r#"
mcp:
  transport: stdio
publishers:
  http_test:
    http:
      url: "http://127.0.0.1:{}"
    description: "Integration Test HTTP"
"#,
        port
    );

    run_integration_test(&config, "publish_to_http_test", None, false);
}

#[test]
fn test_mongodb_integration() {
    let _env = DockerEnvironment::new("mongodb");

    let config = r#"
mcp:
  transport: stdio
publishers:
  mongo_test:
    mongodb:
      url: "mongodb://admin:password@localhost:27017/?authSource=admin"
      database: "testdb"
      collection: "messages"
    description: "Integration Test MongoDB"
consumers:
  mongo_test:
    mongodb:
      url: "mongodb://admin:password@localhost:27017/?authSource=admin"
      database: "testdb"
      collection: "messages"
    description: "Integration Test MongoDB Consumer"
"#;

    run_integration_test(
        config,
        "publish_to_mongo_test",
        Some("consume_from_mongo_test"),
        false,
    );
}

#[test]
fn test_nats_integration() {
    let _env = DockerEnvironment::new("nats");

    let config = r#"
mcp:
  transport: stdio
publishers:
  nats_test:
    nats:
      url: "nats://localhost:4222"
      subject: test-stream.pipeline
      stream: test-stream
      stream_max_messages: 10000
    description: "Integration Test NATS"
consumers:
  nats_test:
    nats:
      url: "nats://localhost:4222"
      subject: test-stream.pipeline
      stream: test-stream
    description: "Integration Test NATS Consumer"
"#;

    run_integration_test(
        config,
        "publish_to_nats_test",
        Some("consume_from_nats_test"),
        false,
    );
}

#[test]
fn test_mqtt_integration() {
    let _env = DockerEnvironment::new("mqtt");

    let config = r#"
mcp:
  transport: stdio
publishers:
  mqtt_test:
    mqtt:
      url: "tcp://localhost:1883"
      topic: "test/topic"
      client_id: "test-publisher-mcp"
      clean_session: false
      qos: 1
      max_inflight: 500
      queue_capacity: 1000
      delayed_ack: false
    description: "Integration Test MQTT"
consumers:
  mqtt_test:
    mqtt:
      url: "tcp://localhost:1883"
      topic: "test/topic"
      client_id: "test-consumer-mcp"
      clean_session: false
      qos: 1
      max_inflight: 500
      queue_capacity: 1000
    description: "Integration Test MQTT Consumer"
"#;

    run_integration_test(
        config,
        "publish_to_mqtt_test",
        Some("consume_from_mqtt_test"),
        true,
    );
}
