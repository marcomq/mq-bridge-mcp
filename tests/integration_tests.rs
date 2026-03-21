use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
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
        let compose_file = Path::new(&root).join("tests").join("docker-compose").join(format!("{}.yml", service)).to_string_lossy().into_owned();

        if !Path::new(&compose_file).exists() {
            eprintln!("Warning: {} not found. Ensure it exists in the project root.", compose_file);
        }

        // Ensure clean state before starting
        let _ = Command::new("docker-compose")
            .args(&["-f", &compose_file, "down", "-v", "--remove-orphans"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        println!("Starting {} environment...", service);
        let status = Command::new("docker-compose")
            .args(&["-f", &compose_file, "up", "-d"])
            .status()
            .expect("Failed to execute docker-compose");

        if !status.success() {
            panic!("Failed to start docker-compose environment for {}", service);
        }

        // Wait for services to be ready.
        wait_for_healthy(&compose_file);
        
        let port = match service {
            "kafka" => 9092,
            "amqp" => 5672,
            "postgres" => 5432,
            _ => 0,
        };
        
        if port > 0 {
            wait_for_port(port);
        }

        Self { compose_file, _guard }
    }
}

impl Drop for DockerEnvironment {
    fn drop(&mut self) {
        println!("Tearing down {} environment...", self.compose_file);
        let _ = Command::new("docker-compose")
            .args(&["-f", &self.compose_file, "down", "-v", "--remove-orphans"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn wait_for_healthy(compose_file: &str) {
    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(30);
    while start.elapsed() < timeout {
        let output = Command::new("docker-compose")
            .args(&["-f", compose_file, "ps"])
            .output()
            .expect("Failed to execute docker-compose ps");

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if stdout.contains("(healthy)") && !stdout.contains("(health: starting)") && !stdout.contains("(unhealthy)") {
                return;
            }
        } else {
            // Docker compose command failed, retry briefly
            return;
        }
        thread::sleep(Duration::from_millis(500));
    }
    panic!("Services in {} did not become healthy in time", compose_file);
}

fn wait_for_port(port: u16) {
    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(30);
    while start.elapsed() < timeout {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(500));
    }
    eprintln!("Warning: Port {} did not become available in 30s", port);
}

// Note: Run these tests with `cargo test -- --test-threads=1 --ignored` to ensure
// sequential execution (avoiding port conflicts) and to include these ignored tests.

fn run_publish_test(config_yaml: &str, tool_name: &str) {
    // Write config to a temporary file
    let config_path = std::env::temp_dir().join(format!("mcp_test_{}.yml", tool_name));
    std::fs::write(&config_path, config_yaml).expect("Failed to write test config");

    let bin_path = env!("CARGO_BIN_EXE_mq-bridge-mcp");

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
    reader.read_line(&mut line).expect("Failed to read init response");
    assert!(line.contains("result"), "Init failed: {}", line);

    // 2. Send Initialized Notification
    let initialized_notif = r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#;
    stdin
        .write_all(format!("{}\n", initialized_notif).as_bytes())
        .expect("Failed to write initialized");

    // 3. Call Publish Tool
    let call_req = format!(
        r#"{{"jsonrpc":"2.0","method":"tools/call","params":{{"name":"{}","arguments":{{"message":{{"payload":"hello from integration test"}}}}}},"id":2}}"#,
        tool_name
    );
    stdin
        .write_all(format!("{}\n", call_req).as_bytes())
        .expect("Failed to write tool call");

    line.clear();
    reader.read_line(&mut line).expect("Failed to read tool response");

    // 4. Assert Success
    // We expect a successful result, not an error.
    if line.contains("\"error\"") {
        panic!("Tool call failed: {}", line);
    }
    assert!(line.contains("Message published successfully"), "Unexpected response: {}", line);

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
      topic: "test_topic"
      delayed_ack: false
    description: "Integration Test Kafka"
"#;

    // Note: Tool name suffix replaces '-' with '_', but here we use 'kafka_test' (underscore) so it stays same.
    run_publish_test(config, "publish_to_kafka_test");
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
      exchange: "amq.topic"
    description: "Integration Test AMQP"
"#;

    run_publish_test(config, "publish_to_amqp_test");
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
    description: "Integration Test Postgres"
"#;

    run_publish_test(config, "publish_to_postgres_test");
}
