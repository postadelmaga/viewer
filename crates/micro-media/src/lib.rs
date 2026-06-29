//! # micro-media — the zero-copy data plane
//!
//! micro has two planes. The **control plane** is the JSON bus ([`micro_bus`]):
//! actions, state, events — small messages that are fine to serialize. The **data plane** is
//! this crate: high-bandwidth payloads (video frames, audio blocks) that must *never* be
//! serialized. A 1080p RGBA frame is ~8 MB; pushing 60 of those per second through
//! `serde_json::Value` is a non-starter. So media moves here instead, by **ownership** —
//! the buffers are `Arc`-backed, so a "send" is a pointer move, not a copy.
//!
//! The two planes cooperate: a producer sends the pixels on a media channel and publishes a
//! tiny "frame ready" control message on the bus; a sink subscribes to the control channel
//! and pulls the actual frame from its media receiver. The bus never sees the bytes.
//!
//! ## Two channel shapes, by real-time intent
//! * [`latest`] — a **single-slot, latest-wins** SPSC mailbox. A new send overwrites an
//!   unread value (the stale frame is dropped). This is what a **video** sink wants: always
//!   render the freshest frame, never accumulate latency behind a slow consumer.
//! * [`bounded`] — a **bounded, lossless** SPSC/MPSC queue with backpressure (a full queue
//!   blocks the producer). This is what an **audio** path wants: every sample block must be
//!   delivered in order; pacing the producer is correct, dropping is not.

use std::sync::{Arc, Condvar, Mutex};

pub mod types;
pub use types::{AudioBlock, Frame, PixelFormat};

// --- latest-wins single-slot mailbox (video) -----------------------------------------------

/// Create a **latest-wins** channel: a one-slot mailbox where a new [`LatestSender::send`]
/// overwrites any value the consumer hasn't taken yet. Single-producer, single-consumer.
pub fn latest<T: Send>() -> (LatestSender<T>, LatestReceiver<T>) {
    let inner = Arc::new(Inner {
        slot: Mutex::new(Slot {
            value: None,
            sender_alive: true,
            receiver_alive: true,
        }),
        ready: Condvar::new(),
    });
    (
        LatestSender {
            inner: inner.clone(),
        },
        LatestReceiver { inner },
    )
}

struct Slot<T> {
    value: Option<T>,
    sender_alive: bool,
    receiver_alive: bool,
}

struct Inner<T> {
    slot: Mutex<Slot<T>>,
    ready: Condvar,
}

/// The producing half of a [`latest`] channel.
pub struct LatestSender<T> {
    inner: Arc<Inner<T>>,
}

/// Returned by [`LatestSender::send`] when the receiver is gone — the value is handed back so
/// the caller can recover it instead of losing it.
#[derive(Debug)]
pub struct Disconnected<T>(pub T);

impl<T: Send> LatestSender<T> {
    /// Put `value` in the slot, dropping whatever unread value was there (latest-wins). Wakes
    /// a waiting receiver. Returns the value back if the receiver has been dropped.
    pub fn send(&self, value: T) -> Result<(), Disconnected<T>> {
        let mut slot = self.inner.slot.lock().unwrap();
        if !slot.receiver_alive {
            return Err(Disconnected(value));
        }
        slot.value = Some(value); // the previous frame, if any, is dropped here
        drop(slot);
        self.inner.ready.notify_one();
        Ok(())
    }
}

impl<T> Drop for LatestSender<T> {
    fn drop(&mut self) {
        let mut slot = self.inner.slot.lock().unwrap();
        slot.sender_alive = false;
        drop(slot);
        // Wake a receiver blocked in `recv` so it can observe the closed channel.
        self.inner.ready.notify_one();
    }
}

/// The consuming half of a [`latest`] channel.
pub struct LatestReceiver<T> {
    inner: Arc<Inner<T>>,
}

impl<T: Send> LatestReceiver<T> {
    /// Block until a value is available, returning `None` once the sender is gone *and* the
    /// slot is empty.
    pub fn recv(&self) -> Option<T> {
        let mut slot = self.inner.slot.lock().unwrap();
        loop {
            if let Some(v) = slot.value.take() {
                return Some(v);
            }
            if !slot.sender_alive {
                return None;
            }
            slot = self.inner.ready.wait(slot).unwrap();
        }
    }

    /// Take the current value without blocking: `Ok(Some)` if one was waiting, `Ok(None)` if
    /// the slot is empty but the sender is alive, `Err(())` if the channel is closed and empty.
    // The `()` error is intentional: "closed" carries no extra information here.
    #[allow(clippy::result_unit_err)]
    pub fn try_recv(&self) -> Result<Option<T>, ()> {
        let mut slot = self.inner.slot.lock().unwrap();
        if let Some(v) = slot.value.take() {
            Ok(Some(v))
        } else if slot.sender_alive {
            Ok(None)
        } else {
            Err(())
        }
    }
}

impl<T> Drop for LatestReceiver<T> {
    fn drop(&mut self) {
        let mut slot = self.inner.slot.lock().unwrap();
        slot.receiver_alive = false;
    }
}

// --- bounded lossless queue (audio) --------------------------------------------------------

pub use std::sync::mpsc::{Receiver as BoundedReceiver, SyncSender as BoundedSender};

/// Create a **bounded, lossless** channel with backpressure: a full queue blocks the producer
/// until the consumer drains it, so nothing is dropped and order is preserved. This is the
/// audio-path shape (every sample block must arrive). It is the std bounded channel, re-exported
/// under the data-plane vocabulary; the receiver has `recv` / `try_recv` / `recv_timeout`.
pub fn bounded<T>(capacity: usize) -> (BoundedSender<T>, BoundedReceiver<T>) {
    std::sync::mpsc::sync_channel(capacity)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_overwrites_unread_value() {
        let (tx, rx) = latest::<i32>();
        tx.send(1).unwrap();
        tx.send(2).unwrap();
        tx.send(3).unwrap(); // 1 and 2 are dropped: only the freshest survives
        assert_eq!(rx.try_recv(), Ok(Some(3)));
        assert_eq!(rx.try_recv(), Ok(None));
    }

    #[test]
    fn latest_recv_blocks_until_sent_then_reports_close() {
        use std::thread;
        use std::time::Duration;

        let (tx, rx) = latest::<i32>();
        let h = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            tx.send(42).unwrap();
            // tx dropped here → channel closes
        });
        assert_eq!(rx.recv(), Some(42));
        assert_eq!(rx.recv(), None); // sender gone, slot empty
        h.join().unwrap();
    }

    #[test]
    fn latest_send_returns_value_when_receiver_gone() {
        let (tx, rx) = latest::<String>();
        drop(rx);
        match tx.send("hi".into()) {
            Err(Disconnected(v)) => assert_eq!(v, "hi"), // recovered, not lost
            Ok(()) => panic!("expected Disconnected"),
        }
    }

    #[test]
    fn bounded_preserves_every_value_in_order() {
        use std::thread;
        use std::time::Duration;

        let (tx, rx) = bounded::<i32>(1); // capacity 1 forces the producer to be paced
        let producer = thread::spawn(move || {
            for i in 0..5 {
                tx.send(i).unwrap();
            }
        });
        thread::sleep(Duration::from_millis(10)); // let the producer block on the full queue
        let mut got = Vec::new();
        for _ in 0..5 {
            got.push(rx.recv().unwrap());
        }
        assert_eq!(got, vec![0, 1, 2, 3, 4]); // nothing dropped, order kept
        producer.join().unwrap();
    }
}
