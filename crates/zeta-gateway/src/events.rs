//! Event bus: every event is written to the event log first, then broadcast to
//! live listeners. A client that was offline replays the log from the last `seq`
//! it saw, so it never misses anything.

use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::broadcast;

use crate::model::{now_ms, StoredEvent};
use crate::store::Store;

#[derive(Clone)]
pub struct EventBus {
    store: Arc<Store>,
    tx: broadcast::Sender<StoredEvent>,
    /// Keeps "append, then broadcast" atomic so live listeners see increasing `seq`.
    order: Arc<Mutex<()>>,
}

impl EventBus {
    pub fn new(store: Arc<Store>) -> Self {
        let (tx, _) = broadcast::channel(256);
        Self {
            store,
            tx,
            order: Arc::new(Mutex::new(())),
        }
    }

    pub fn publish(&self, kind: &str, payload: Value) -> rusqlite::Result<StoredEvent> {
        let _order = self.order.lock().unwrap_or_else(|e| e.into_inner());
        let created_at = now_ms();
        let seq = self.store.append_event(kind, &payload, created_at)?;
        let event = StoredEvent {
            seq,
            kind: kind.to_string(),
            payload,
            created_at,
        };
        // No live listener is fine: the event is in the log.
        let _ = self.tx.send(event.clone());
        Ok(event)
    }

    /// Broadcast an event that is not kept in the log (upload progress…). It has
    /// `seq` 0; listeners that are not connected miss it.
    pub fn notify(&self, kind: &str, payload: Value) {
        let event = StoredEvent {
            seq: 0,
            kind: kind.to_string(),
            payload,
            created_at: now_ms(),
        };
        let _ = self.tx.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<StoredEvent> {
        self.tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn published_events_are_logged_and_broadcast() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let bus = EventBus::new(store.clone());
        let mut rx = bus.subscribe();
        let ev = bus.publish("message", json!({ "a": 1 })).unwrap();
        assert_eq!(rx.recv().await.unwrap(), ev);
        assert_eq!(store.events_since(0, 10).unwrap(), vec![ev.clone()]);

        bus.notify("upload", json!({ "b": 2 }));
        let transient = rx.recv().await.unwrap();
        assert_eq!((transient.seq, transient.kind.as_str()), (0, "upload"));
        assert_eq!(store.events_since(0, 10).unwrap(), vec![ev], "not logged");
    }
}
