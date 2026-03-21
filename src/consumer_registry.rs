use mq_bridge::traits::MessageConsumer;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};
use tokio::sync::Mutex;

type ConsumerArc = Arc<Mutex<Box<dyn MessageConsumer>>>;
static ACTIVE_CONSUMERS: OnceLock<RwLock<HashMap<String, ConsumerArc>>> = OnceLock::new();

/// Retrieves a registered active consumer by name.
pub fn get_consumer(name: &str) -> Option<Arc<Mutex<Box<dyn MessageConsumer>>>> {
    let registry = ACTIVE_CONSUMERS.get_or_init(|| RwLock::new(HashMap::new()));
    let reader = registry.read().expect("Active consumers lock poisoned");
    reader.get(name).cloned()
}

/// Registers an active consumer.
pub fn register_consumer(
    name: &str,
    consumer: Box<dyn MessageConsumer>,
) -> Arc<Mutex<Box<dyn MessageConsumer>>> {
    let registry = ACTIVE_CONSUMERS.get_or_init(|| RwLock::new(HashMap::new()));
    let mut writer = registry.write().expect("Active consumers lock poisoned");
    let consumer = Arc::new(Mutex::new(consumer));
    writer.insert(name.to_string(), consumer.clone());
    consumer
}
