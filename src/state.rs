use serde_json::{Value, json};
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct ReportedState {
    pub generation: u64,
    pub connected: bool,
    pub status: Option<Value>,
    pub updated: Option<Instant>,
}
impl ReportedState {
    pub fn connect(&mut self, generation: u64) {
        if generation > self.generation {
            self.generation = generation;
            self.connected = true;
            self.updated = None;
            self.status = None;
        }
    }
    pub fn disconnect(&mut self, generation: u64) {
        if generation == self.generation {
            self.connected = false;
        }
    }
    pub fn update(&mut self, generation: u64, value: Value, now: Instant) {
        if self.connected && generation == self.generation && value.is_object() {
            self.status = Some(value);
            self.updated = Some(now);
        }
    }
    pub fn fresh(&self, now: Instant) -> bool {
        self.connected
            && self
                .updated
                .is_some_and(|t| now.saturating_duration_since(t) <= Duration::from_secs(30))
    }
    pub fn snapshot(&self, now: Instant) -> Value {
        json!({
            "online": self.fresh(now),
            "connected": self.connected,
            "age_seconds": self.updated.map(|t| now.saturating_duration_since(t).as_secs()),
            "status": self.status,
        })
    }
}


