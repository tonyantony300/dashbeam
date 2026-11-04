//! Bridges engine events into the agent's event loop.

use engine::EventEmitter;
use tokio::sync::mpsc;

/// `(event name, optional JSON payload)` as emitted by the engine.
pub type EngineEvent = (String, Option<String>);

/// [`EventEmitter`] that forwards every engine event onto an unbounded channel.
pub struct ChannelEmitter {
    tx: mpsc::UnboundedSender<EngineEvent>,
}

impl ChannelEmitter {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<EngineEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { tx }, rx)
    }

    fn push(&self, name: &str, payload: Option<&str>) -> Result<(), String> {
        self.tx
            .send((name.to_string(), payload.map(str::to_string)))
            .map_err(|_| "agent event loop stopped".to_string())
    }
}

impl EventEmitter for ChannelEmitter {
    fn emit_event(&self, name: &str) -> Result<(), String> {
        self.push(name, None)
    }

    fn emit_event_with_payload(&self, name: &str, payload: &str) -> Result<(), String> {
        self.push(name, Some(payload))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forwards_events_in_order() {
        let (emitter, mut rx) = ChannelEmitter::new();
        emitter.emit_event("device-paired").unwrap();
        emitter
            .emit_event_with_payload("transfer-progress", "{\"x\":1}")
            .unwrap();
        assert_eq!(rx.try_recv().unwrap(), ("device-paired".into(), None));
        assert_eq!(
            rx.try_recv().unwrap(),
            ("transfer-progress".into(), Some("{\"x\":1}".into()))
        );
    }

    #[test]
    fn errors_once_receiver_dropped() {
        let (emitter, rx) = ChannelEmitter::new();
        drop(rx);
        assert!(emitter.emit_event("device-paired").is_err());
    }
}
