//! A browser tab as a display, for a program running IN the tab (the `brood-tab` wasm
//! build): the page's script, not a socket, is the other end. The program posts display
//! events — a frame, a texture, a sound, window options, each a name and a JSON payload,
//! the shapes `b2d-web` already streams over SSE — into an outbox the page drains after
//! each pump; the page hands each input message (a Brood form as text, read by the
//! program, never evaluated) to the one process that subscribed for it.
//!
//! Mechanism only, and not wasm-specific: on a native build nothing drains the outbox,
//! which is what lets a test drive the Brood side without a browser.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// The most events the outbox holds before it drops the oldest: a program whose page
/// stopped draining (a hidden tab) must not grow it without bound.
const OUTBOX_LIMIT: usize = 4096;

static OUTBOX: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

/// The pid input is delivered to; 0 when no process has subscribed.
static SUBSCRIBER: AtomicU64 = AtomicU64::new(0);

/// Queue display event `event` with payload `data` for the page.
pub fn post(event: String, data: String) {
    let mut outbox = crate::core::sync::lock(&OUTBOX);
    if outbox.len() >= OUTBOX_LIMIT {
        outbox.remove(0);
    }
    outbox.push((event, data));
}

/// Every event posted since the last call, oldest first.
pub fn take_events() -> Vec<(String, String)> {
    std::mem::take(&mut *crate::core::sync::lock(&OUTBOX))
}

/// Make `pid` the process the page's input goes to (the last subscriber wins).
pub fn subscribe(pid: u64) {
    SUBSCRIBER.store(pid, Ordering::Release);
}

/// Hand `text` — one input message the page read off a key, a click, a resize — to the
/// subscriber as a string; whether there was one to take it.
pub fn deliver_input(text: &str) -> bool {
    match SUBSCRIBER.load(Ordering::Acquire) {
        0 => false,
        pid => {
            crate::process::deliver(pid, crate::process::Message::Str(text.to_string()));
            true
        }
    }
}

/// Drive the program for at most `budget`: run every runnable process, firing the receive
/// timeouts that are due on the wall clock, and return how long until the next one is
/// (`None` with none pending; zero when the budget ran out with work left). The page calls
/// this after each input and whenever the returned delay elapses.
#[cfg(target_arch = "wasm32")]
pub fn pump(budget: std::time::Duration) -> Option<std::time::Duration> {
    crate::process::pump_due(budget)
}

/// Off wasm the worker threads run every program; there is nothing to drive.
#[cfg(not(target_arch = "wasm32"))]
pub fn pump(_budget: std::time::Duration) -> Option<std::time::Duration> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // One test, not several: the outbox and the subscriber are process-wide.
    #[test]
    fn events_drain_in_order_and_input_needs_a_subscriber() {
        take_events();
        post("frame".into(), "[]".into());
        post("window".into(), "{}".into());
        assert_eq!(
            take_events(),
            vec![
                ("frame".into(), "[]".into()),
                ("window".into(), "{}".into())
            ]
        );
        assert!(take_events().is_empty());
        for n in 0..OUTBOX_LIMIT + 10 {
            post("frame".into(), n.to_string());
        }
        let kept = take_events();
        assert_eq!(kept.len(), OUTBOX_LIMIT);
        assert_eq!(kept[0].1, "10", "the oldest are the ones dropped");
        SUBSCRIBER.store(0, Ordering::Release);
        assert!(!deliver_input("[:resize 1 1]"));
    }
}
