//! Per-process CPU time: how long a process actually ran, as against how long it took.
//!
//! The question it answers is the one wall-clock timing cannot: was a slow step slow
//! because its code did a lot of work, or because the process was WAITING — for a worker,
//! behind other Brood processes, or behind other programs on the machine? An editor's
//! update that reads 25 ms on the wall and 2 ms here was starved, not slow, and the fix is
//! nowhere near the code that was timed.
//!
//! A green process's time is the sum of its quanta, each measured on the worker thread
//! that ran it (the thread's own CPU clock, so time the OS gave to another thread does not
//! count). A ROOT context — a REPL or file-runner thread that is not scheduled in quanta —
//! is pinned to its OS thread for life, so its figure is that thread's CPU clock, read
//! directly. Off Unix there is no per-thread clock to read, and both answer 0.
//!
//! **Off until someone asks.** Reading a thread's CPU clock costs ~155 ns, two reads per
//! quantum, and a message-handling process can run quanta of a few microseconds — a
//! 5–15% tax on exactly the code that is most sensitive to it, paid by every program for a
//! diagnostic most never read. So quantum accounting is ARMED by the first read of
//! `:cpu-us` and costs one relaxed load per quantum until then. A green process's figure
//! therefore counts from that moment, which is all a delta needs; a root context's thread
//! clock is exact from the start either way.

use std::sync::atomic::{AtomicBool, Ordering};

static ARMED: AtomicBool = AtomicBool::new(false);

/// Whether quanta are being timed — checked at each quantum's start.
pub(crate) fn armed() -> bool {
    ARMED.load(Ordering::Relaxed)
}

/// Start timing quanta, from now on — the first read of `:cpu-us` calls this.
pub(crate) fn arm() {
    ARMED.store(true, Ordering::Relaxed);
}

/// The calling thread's consumed CPU time in nanoseconds (0 where unsupported).
#[cfg(unix)]
pub(crate) fn thread_cpu_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable timespec; CLOCK_THREAD_CPUTIME_ID needs no handle.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    if rc == 0 {
        ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
    } else {
        0
    }
}

#[cfg(not(unix))]
pub(crate) fn thread_cpu_ns() -> u64 {
    0
}

/// A clock id for the CALLING thread's CPU time that any thread may read later — how a
/// root context, which lives on one thread, is measured from elsewhere (`process-info`
/// asked by another process). `None` where the platform has no such clock.
#[cfg(target_os = "linux")]
pub(crate) fn current_thread_clock() -> Option<i64> {
    let mut clock: libc::clockid_t = 0;
    // SAFETY: `pthread_self()` is always valid for the calling thread; `clock` is writable.
    let rc = unsafe { libc::pthread_getcpuclockid(libc::pthread_self(), &mut clock) };
    if rc == 0 { Some(clock as i64) } else { None }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn current_thread_clock() -> Option<i64> {
    None
}

/// Read a clock from [`current_thread_clock`], in nanoseconds. `None` if the thread is gone
/// (the clock is then invalid) or the platform has none.
#[cfg(target_os = "linux")]
pub(crate) fn clock_ns(clock: i64) -> Option<u64> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is writable; an invalid clock id makes the call fail, not misbehave.
    let rc = unsafe { libc::clock_gettime(clock as libc::clockid_t, &mut ts) };
    if rc == 0 {
        Some(ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64)
    } else {
        None
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn clock_ns(_clock: i64) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn thread_cpu_time_advances_with_work_and_not_with_sleep() {
        let t0 = thread_cpu_ns();
        let mut x = 0u64;
        for i in 0..5_000_000u64 {
            x = x.wrapping_add(i * i);
        }
        std::hint::black_box(x);
        let worked = thread_cpu_ns() - t0;
        let t1 = thread_cpu_ns();
        std::thread::sleep(std::time::Duration::from_millis(30));
        let slept = thread_cpu_ns() - t1;
        assert!(worked > 0, "work consumed no CPU time");
        assert!(slept < 5_000_000, "30 ms asleep charged {slept} ns of CPU");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn another_thread_reads_a_root_threads_clock() {
        let (tx, rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let h = std::thread::spawn(move || {
            let mut x = 0u64;
            for i in 0..5_000_000u64 {
                x = x.wrapping_add(i * i);
            }
            std::hint::black_box(x);
            tx.send(current_thread_clock().unwrap()).unwrap();
            done_rx.recv().unwrap(); // stay alive while it is read
        });
        let clock = rx.recv().unwrap();
        assert!(clock_ns(clock).unwrap() > 0);
        done_tx.send(()).unwrap();
        h.join().unwrap();
    }
}

#[cfg(test)]
mod cost {
    /// What one read costs — the price per quantum is two of these. `--ignored` to run.
    #[cfg(unix)]
    #[test]
    #[ignore]
    fn thread_cpu_ns_cost() {
        let n = 1_000_000u32;
        let t = std::time::Instant::now();
        let mut s = 0u64;
        for _ in 0..n {
            s = s.wrapping_add(super::thread_cpu_ns());
        }
        std::hint::black_box(s);
        eprintln!(
            "thread_cpu_ns: {} ns/call",
            t.elapsed().as_nanos() / n as u128
        );
    }
}
