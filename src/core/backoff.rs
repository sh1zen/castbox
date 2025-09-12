use core::cell::Cell;
use core::fmt;
use std::{hint, thread};
use std::hint::spin_loop;

const SPIN_LIMIT: u32 = 6;
const YIELD_LIMIT: u32 = 10;

/// Exponential backoff helper for spin- and yield-based waiting.
///
/// Backing off in retry loops reduces contention and can improve overall throughput,
/// especially under medium contention.
///
/// Backoff starts by issuing lightweight processor hints via [core::hint::spin_loop]
/// and gradually escalates to yielding the current thread to the OS scheduler
/// via [std::thread::yield_now]. After enough steps, it is considered "completed"
/// and callers are advised to switch to a blocking primitive instead.
/// Each step takes roughly twice as long as the previous one.
///
/// Example
/// ```rust
/// let backoff = Backoff::new();
/// loop {
///     if try_do_something() {
///         backoff.reset();
///         break;
///     }
///     backoff.snooze();
///     if backoff.is_completed() {
///         park_or_block();
///     }
/// }
/// ```
pub(crate) struct Backoff {
    step: Cell<u32>,
}
impl Backoff {
    /// Creates a new `Backoff` starting at step 0.
    ///
    /// This is a lightweight, non-allocating operation intended to be used per
    /// wait/retry loop (do not share a single instance between threads).
    pub(crate) fn new() -> Self {
        Backoff { step: Cell::new(0) }
    }

    /// Resets the internal step counter to `0`.
    ///
    /// Call this after making progress so subsequent waits start from the
    /// shortest delay again.
    #[inline]
    pub(crate) fn reset(&self) {
        self.step.set(0);
    }

    /// Performs one exponential backoff step suitable for lock-free retry loops.
    ///
    /// Use this when another thread likely made progress and we want to retry soon.
    /// This issues [core::hint::spin_loop] hints up to `SPIN_LIMIT` and advances the
    /// internal step counter (capped for spinning). It never yields the thread to
    /// the OS; for that, prefer [`snooze`].
    #[inline]
    pub(crate) fn spin(&self) {
        for _ in 0..1 << self.step.get().min(SPIN_LIMIT) {
            hint::spin_loop();
        }

        if self.step.get() <= SPIN_LIMIT {
            self.step.set(self.step.get() + 1);
        }
    }

    /// Skips directly to the yielding phase of the backoff.
    ///
    /// Sets the internal step just past the spinning threshold so that subsequent
    /// calls to [`snooze`] will yield the current thread to the OS. Useful when
    /// additional short spins are unlikely to help.
    #[inline]
    pub(crate) fn yielder(&self) {
        self.step.set(SPIN_LIMIT + 1);
    }

    /// Performs one backoff step that may yield the current thread.
    ///
    /// Use this in blocking/wait loops when we are waiting for another thread to make progress.
    /// While `step <= SPIN_LIMIT`, this behaves like [`spin`] (CPU pause/hints). After that,
    /// it yields the current thread via [std::thread::yield_now]. The step counter advances up to
    /// `YIELD_LIMIT`.
    ///
    /// If possible, use [`is_completed`] to decide when to stop using backoff and switch to
    /// a blocking synchronization primitive instead.
    #[inline]
    pub(crate) fn snooze(&self) {
        if self.step.get() <= SPIN_LIMIT {
            for _ in 0..1 << self.step.get() {
                hint::spin_loop();
            }
        } else {
            thread::yield_now();
        }

        if self.step.get() <= YIELD_LIMIT {
            self.step.set(self.step.get() + 1);
        }
    }

    /// Returns `true` once backoff has exceeded `YIELD_LIMIT` and blocking is advised.
    ///
    /// At this point, prefer parking the thread or using a different synchronization
    /// mechanism instead of continuing to spin/yield.
    #[inline]
    pub(crate) fn is_completed(&self) -> bool {
        self.step.get() > YIELD_LIMIT
    }

    /// Returns `true` once the backoff has reached the yielding phase (`step >= SPIN_LIMIT`).
    ///
    /// When this is `true`, subsequent calls to [`snooze`] will yield the current
    /// thread to the OS scheduler.
    #[inline]
    pub(crate) fn is_yielding(&self) -> bool {
        self.step.get() >= SPIN_LIMIT
    }

    #[inline]
    pub(crate) fn cpu_relax(mut counter: u32) {
        if counter >= 10 {
            return;
        }

        counter += 1;

        if counter <= 3 {
            for _ in 0..(1 << counter) {
                spin_loop();
            }
        } else {
            thread::yield_now();
        }
    }
}

impl fmt::Debug for Backoff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Backoff")
            .field("step", &self.step)
            .field("is_completed", &self.is_completed())
            .finish()
    }
}

impl Default for Backoff {
    fn default() -> Backoff {
        Backoff::new()
    }
}
