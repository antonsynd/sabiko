//! Helpers shared by the `sabiko_driver` integration tests.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

/// Runs `job` over `items` on one thread per core (at most eight), collecting one result
/// per item, in order. Cargo runs test binaries one after another, so a corpus-wide test
/// otherwise uses one core of the machine while it compiles every program: the
/// `sabiko_types` mutants run these suites once per mutant (#165).
///
/// A panic in `job` propagates once every thread has stopped, as "a scoped thread
/// panicked"; catch it inside `job` (`std::panic::catch_unwind`, [`panic_message`]) to
/// report which item it came from.
pub fn parallel_map<T: Sync, R: Send>(items: &[T], job: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let workers = std::thread::available_parallelism()
        .map_or(2, std::num::NonZeroUsize::get)
        .clamp(1, 8);
    let next = AtomicUsize::new(0);
    let results: Vec<Mutex<Option<R>>> = items.iter().map(|_| Mutex::new(None)).collect();
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(item) = items.get(i) else { break };
                let result = job(item);
                *results[i].lock().unwrap() = Some(result);
            });
        }
    });
    results
        .into_iter()
        .map(|m| m.into_inner().unwrap().expect("every item gets a result"))
        .collect()
}

/// The message of a caught panic: its `&str` or `String` payload.
pub fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic without a message".to_owned())
}
