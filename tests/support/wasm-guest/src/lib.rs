use std::cell::Cell;
use std::sync::atomic::{AtomicU32, Ordering};

wit_bindgen::generate!({ world: "guest", path: "wit" });

use exports::brood::fixture::counter::{Guest as CounterGuest, GuestCounter};
use exports::brood::fixture::host::Guest as HostGuest;

static DROPPED: AtomicU32 = AtomicU32::new(0);

struct Fixture;

struct Counter {
    value: Cell<i32>,
}

impl GuestCounter for Counter {
    fn new(start: i32) -> Self {
        Counter {
            value: Cell::new(start),
        }
    }

    fn bump(&self) -> i32 {
        self.value.set(self.value.get() + 1);
        self.value.get()
    }

    fn consume(counter: exports::brood::fixture::counter::Counter) -> i32 {
        counter.get::<Counter>().value.get()
    }
}

impl Drop for Counter {
    fn drop(&mut self) {
        DROPPED.fetch_add(1, Ordering::SeqCst);
    }
}

impl CounterGuest for Fixture {
    type Counter = Counter;

    fn dropped() -> u32 {
        DROPPED.load(Ordering::SeqCst)
    }
}

impl HostGuest for Fixture {
    fn read_file(path: String) -> Result<String, String> {
        std::fs::read_to_string(path).map_err(|error| error.to_string())
    }

    fn write_file(path: String, text: String) -> Result<(), String> {
        std::fs::write(path, text).map_err(|error| error.to_string())
    }

    fn env_var(name: String) -> Option<String> {
        std::env::var(name).ok()
    }

    fn argument_count() -> u32 {
        std::env::args().count() as u32
    }

    fn clock_moves() -> bool {
        let start = std::time::Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(1));
        start.elapsed().as_nanos() > 0
    }
}

export!(Fixture);
