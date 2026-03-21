use mq_bridge::Publisher;
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};
use tracing::debug;

#[derive(Clone)]
pub struct PublisherDefinition {
    pub publisher: Publisher,
    pub description: String,
    pub endpoint_type: String,
    pub destructive: bool,
    pub open_world: bool,
    pub idempotent: bool,
}

static PUBLISHER_DEFINITION_REGISTRY: OnceLock<RwLock<HashMap<String, PublisherDefinition>>> =
    OnceLock::new();

/// Registers a named publisher definition (a route with `input: null`).
/// This allows dynamic discovery of publishers for tools like MCP.
pub fn register_publisher(name: &str, publisher: PublisherDefinition) {
    let registry = PUBLISHER_DEFINITION_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()));
    let mut writer = registry
        .write()
        .expect("Publisher definition registry lock poisoned");
    if writer.insert(name.to_string(), publisher).is_some() {
        debug!(
            "Overwriting a registered publisher definition named '{}'",
            name
        );
    }
}

/// Retrieves a registered publisher definition by name.
pub fn get_publisher(name: &str) -> Option<PublisherDefinition> {
    let registry = PUBLISHER_DEFINITION_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()));
    let reader = registry
        .read()
        .expect("Publisher definition registry lock poisoned");
    reader.get(name).cloned()
}

/// Returns a list of all registered publisher definition names.
pub fn list_publishers() -> Vec<String> {
    let registry = PUBLISHER_DEFINITION_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()));
    let reader = registry
        .read()
        .expect("Publisher definition registry lock poisoned");
    reader.keys().cloned().collect()
}
