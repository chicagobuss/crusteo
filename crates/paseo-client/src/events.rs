//! Bounded handoff from the socket task to its UI consumer. Both event count
//! and estimated owned payload bytes are limited: a paused consumer must not
//! turn a healthy socket into an ever-growing transcript of daemon pushes.

use std::mem::size_of;
use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::proto::json_bytes;
use crate::{ConnState, Event};

pub(crate) const EVENT_LIMIT: usize = 64;
const UNIT_BYTES: usize = 4096;
const PAYLOAD_UNITS: usize = 2048; // 8 MiB of estimated owned payloads.

struct Buffered {
    event: Event,
    _budget: OwnedSemaphorePermit,
}

#[derive(Debug)]
pub(crate) struct ConsumerGone;

// The socket loop acquires budget and publishes in sequence; do not derive Clone.
pub(crate) struct EventSender {
    queue: async_channel::Sender<Buffered>,
    budget: Arc<Semaphore>,
}

/// The ordered, memory-bounded event stream of one daemon connection.
/// Clones share a queue; give it to one consumer. Receiving releases the
/// queue's budget; the caller owns the returned event until it is processed.
/// Drain independently of RPC requests: backpressure pauses the socket loop,
/// including request replies, outgoing commands and keepalive. Do not await a
/// request reply inside the consuming loop; a long stall may cause the daemon
/// to disconnect, after which the client reconnects and the app resynchronizes.
#[derive(Clone)]
pub struct EventReceiver {
    queue: async_channel::Receiver<Buffered>,
    budget: Arc<Semaphore>,
}

pub(crate) fn channel() -> (EventSender, EventReceiver) {
    let (tx, rx) = async_channel::bounded(EVENT_LIMIT);
    let budget = Arc::new(Semaphore::new(PAYLOAD_UNITS));
    (
        EventSender {
            queue: tx,
            budget: budget.clone(),
        },
        EventReceiver { queue: rx, budget },
    )
}

impl EventSender {
    pub(crate) async fn send(&self, event: Event) -> Result<(), ConsumerGone> {
        // A payload larger than the budget is allowed alone. There is still
        // at most one additional event being decoded by this socket task.
        // This is backpressure, not dropping timeline or terminal events.
        let units = event_bytes(&event)
            .div_ceil(UNIT_BYTES)
            .clamp(1, PAYLOAD_UNITS);
        let permit = self
            .budget
            .clone()
            .acquire_many_owned(units as u32)
            .await
            .map_err(|_| ConsumerGone)?;
        self.queue
            .send(Buffered {
                event,
                _budget: permit,
            })
            .await
            .map_err(|_| ConsumerGone)
    }
}

impl EventReceiver {
    /// Waits for the next event, without requiring a Tokio runtime.
    pub async fn recv(&self) -> Result<Event, async_channel::RecvError> {
        self.queue.recv().await.map(|buffered| buffered.event)
    }

    /// Number of events waiting for the consumer.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Whether the queue is currently empty.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Cancels blocked producers, including those waiting for byte budget.
    /// Buffered events remain readable. Called when the last client drops.
    pub(crate) fn shutdown(&self) {
        self.budget.close();
        self.queue.close();
    }
}

fn event_bytes(event: &Event) -> usize {
    let payload = match event {
        Event::Message(value) => json_bytes(value),
        Event::Binary(bytes) => bytes.capacity(),
        Event::State(ConnState::Connected(info)) => info.allocated_bytes(),
        Event::State(ConnState::Disconnected { reason, .. }) => reason.capacity(),
        Event::State(ConnState::Connecting { .. }) => 0,
    };
    size_of::<Event>().saturating_add(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn stalled_consumer_has_a_count_limit_and_preserves_event_order() {
        crate::block_on(async {
            let (tx, rx) = channel();
            for n in 0..EVENT_LIMIT {
                tx.send(Event::Message(serde_json::json!({ "n": n })))
                    .await
                    .unwrap();
            }
            let send = tx.send(Event::Binary(vec![42]));
            tokio::pin!(send);
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(20), &mut send)
                    .await
                    .is_err()
            );
            assert_eq!(rx.len(), EVENT_LIMIT);
            for n in 0..EVENT_LIMIT {
                let Event::Message(value) = rx.recv().await.unwrap() else {
                    panic!("out of order")
                };
                assert_eq!(value["n"], n);
            }
            send.await.unwrap();
            assert!(matches!(rx.recv().await.unwrap(), Event::Binary(bytes) if bytes == [42]));
        });
    }

    #[test]
    fn large_payloads_backpressure_before_the_event_count_limit() {
        crate::block_on(async {
            let (tx, rx) = channel();
            // An oversized event occupies the whole budget without breaking
            // compatibility with large daemon messages.
            tx.send(Event::Message(Value::String(
                "x".repeat(UNIT_BYTES * PAYLOAD_UNITS),
            )))
            .await
            .unwrap();
            let send = tx.send(Event::Binary(vec![1]));
            tokio::pin!(send);
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(20), &mut send)
                    .await
                    .is_err()
            );
            assert_eq!(rx.len(), 1);
            drop(rx.recv().await.unwrap());
            send.await.unwrap();
            assert!(matches!(rx.recv().await.unwrap(), Event::Binary(bytes) if bytes == [1]));
        });
    }

    #[test]
    fn closing_cancels_both_count_and_byte_budget_waiters() {
        crate::block_on(async {
            for large in [false, true] {
                let (tx, rx) = channel();
                if large {
                    tx.send(Event::Binary(vec![0; UNIT_BYTES * PAYLOAD_UNITS]))
                        .await
                        .unwrap();
                } else {
                    for _ in 0..EVENT_LIMIT {
                        tx.send(Event::Binary(vec![0])).await.unwrap();
                    }
                }
                let send = tx.send(Event::Binary(vec![1]));
                tokio::pin!(send);
                assert!(
                    tokio::time::timeout(std::time::Duration::from_millis(20), &mut send)
                        .await
                        .is_err()
                );
                rx.shutdown();
                assert!(send.await.is_err());
                assert!(rx.recv().await.is_ok()); // closing retains queued events
            }
        });
    }
}
