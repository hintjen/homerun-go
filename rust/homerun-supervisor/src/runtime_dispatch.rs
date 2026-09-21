//! Putting work on the engine's runtime from a thread that may already be on
//! one.
//!
//! `Handle::block_on` panics — "Cannot start a runtime from within a runtime"
//! — when the calling thread is already driving async tasks. That is not a
//! corner: this crate runs **two** runtimes, the linked engine's
//! ([`crate::pumpkin_engine`]) and the device websocket's
//! ([`crate::device_ws`]), and a console command can arrive through either.
//!
//! - From the app, a command comes down the C ABI on a host thread. Nothing
//!   has entered a runtime, so blocking until the dispatch lands is free and
//!   the caller learns whether it ran.
//! - From the dashboard, the same command arrives as `Request::Rcon` on a
//!   *device websocket task*. That thread is inside a runtime, and the same
//!   `block_on` panics before the command reaches the server.
//!
//! The second case shipped: an iPhone hosting a Pumpkin server answered every
//! command typed into the dashboard's console with a Tokio panic, caught at
//! the ABI and shown to the player verbatim. It could not happen on Android,
//! where the engine is a child process and `command` writes to its stdin.
//!
//! So the decision is made here, once, rather than assumed at each call site:
//! **spawn always, wait only when waiting is legal.**

use std::future::Future;

use tokio::runtime::Handle;

/// Run `task` on `runtime`, waiting for it only when the caller may block.
///
/// The task is spawned either way, which is what keeps a panicking command off
/// the caller's thread — the task boundary catches it. What changes is whether
/// we wait for the join handle:
///
/// - **Outside a runtime** — wait, and report a panicking task as an error.
///   This is the host-thread path and it behaves exactly as it always has.
/// - **Inside a runtime** — return as soon as the work is queued. Blocking
///   here is the panic this module exists to prevent, and there is nothing to
///   gain by it: every reply a command produces goes to the console, which the
///   dashboard and the app both read from the log rather than from this return
///   value.
///
/// `Err` therefore means "the task ran and panicked", never "we could not tell
/// whether it ran" — the caller gets `Ok` in the second case, because queued
/// work on a live runtime is as accepted as a command gets.
pub(crate) fn dispatch<F>(runtime: &Handle, task: F) -> Result<(), ()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let joined = runtime.spawn(task);

    // `try_current` is the whole test, and it asks the right question: not
    // "which runtime is this" but "is *this thread* inside one", which is
    // exactly what `block_on` refuses. A second runtime's worker is as fatal
    // as the engine's own.
    if Handle::try_current().is_ok() {
        return Ok(());
    }

    runtime.block_on(joined).map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use tokio::runtime::Builder;

    fn runtime() -> tokio::runtime::Runtime {
        Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("a test runtime")
    }

    /// The bug, as it shipped: a command dispatched from a thread that is
    /// already inside a runtime. Before this module, the `block_on` at this
    /// point panicked with "Cannot start a runtime from within a runtime" and
    /// the command never reached the server.
    ///
    /// The outer runtime is deliberately *not* the one the work goes to —
    /// that is the real shape, where the device websocket's runtime hands work
    /// to the engine's.
    #[test]
    fn dispatching_from_inside_another_runtime_does_not_panic() {
        let engine = runtime();
        let websocket = runtime();
        // The handle, not the runtime: moving a `Runtime` into the async block
        // would drop it there, and Tokio refuses that for its own reasons.
        let handle = engine.handle().clone();

        let ran = Arc::new(AtomicBool::new(false));
        let flag = ran.clone();

        let outcome = websocket.block_on(async move {
            // Inside a runtime now, which is what `Request::Rcon` is.
            dispatch(&handle, async move {
                flag.store(true, Ordering::SeqCst);
            })
        });

        assert_eq!(outcome, Ok(()), "a queued command is an accepted command");

        // The work is queued, not necessarily finished, so give the engine's
        // runtime a moment to run it — the point of the assertion is that the
        // task reached the runtime at all.
        for _ in 0..200 {
            if ran.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("the task never ran on the engine's runtime");
    }

    /// The host-thread path, unchanged: outside any runtime the caller waits,
    /// so by the time `dispatch` returns the work is done.
    #[test]
    fn dispatching_from_outside_a_runtime_waits_for_the_task() {
        let engine = runtime();
        let ran = Arc::new(AtomicBool::new(false));
        let flag = ran.clone();

        let outcome = dispatch(engine.handle(), async move {
            flag.store(true, Ordering::SeqCst);
        });

        assert_eq!(outcome, Ok(()));
        assert!(
            ran.load(Ordering::SeqCst),
            "outside a runtime the caller waits, so the task has already run"
        );
    }

    /// A command that panics must be reported, not swallowed — and must not
    /// take the caller's thread with it.
    #[test]
    fn a_panicking_task_is_an_error_when_we_waited_for_it() {
        // Intentional task panics run the process-global crash hook too.
        // Hold the same guard as its tests until the runtime has shut down.
        let _guard = crate::crash::test_guard();
        let engine = runtime();

        let outcome = dispatch(engine.handle(), async {
            panic!("a command that reached for something that was not there");
        });

        assert_eq!(outcome, Err(()));
    }

    /// The same panicking task, dispatched from inside a runtime: we did not
    /// wait, so there is nothing to report and the caller is told the command
    /// was accepted. The panic stays inside the task.
    #[test]
    fn a_panicking_task_cannot_be_reported_when_we_did_not_wait() {
        let _guard = crate::crash::test_guard();
        let engine = runtime();
        let websocket = runtime();
        let handle = engine.handle().clone();

        let outcome = websocket.block_on(async move {
            dispatch(&handle, async {
                panic!("still contained by the task boundary");
            })
        });

        assert_eq!(outcome, Ok(()));
    }
}
