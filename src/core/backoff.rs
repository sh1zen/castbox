use core::cell::Cell;
use core::fmt;
use std::sync::OnceLock;
use std::{hint, thread};

static ADAPTIVE_BACKOFF: OnceLock<usize> = OnceLock::new();

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
    step: Cell<usize>,
    spin_limit: usize,
    yield_limit: usize,
}

impl Backoff {
    /// Creates a new `Backoff` starting at step 0.
    ///
    /// This is a lightweight, non-allocating operation intended to be used per
    /// wait/retry loop (do not share a single instance between threads).
    pub(crate) fn new() -> Self {
        let adaptive = *ADAPTIVE_BACKOFF.get_or_init(|| {
            thread::available_parallelism()
                .map(|p| p.get())
                .unwrap_or(4)
        });

        let spin_limit = match adaptive {
            1..=2 => 6,
            3..=8 => 8,
            9..=32 => 10,
            _ => 12,
        };

        let yield_limit = match adaptive {
            1..=2 => 2,
            3..=8 => 4,
            9..=32 => 6,
            _ => 10,
        } + spin_limit;

        Backoff {
            step: Cell::new(0),
            spin_limit,
            yield_limit,
        }
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
        let spins = self.step.get();
        for _ in 0..1 << spins.min(self.spin_limit) {
            hint::spin_loop();
        }

        if spins <= self.spin_limit {
            self.step.set(spins + 1);
        }
    }

    /// Skips directly to the yielding phase of the backoff.
    ///
    /// Sets the internal step just past the spinning threshold so that subsequent
    /// calls to [`snooze`] will yield the current thread to the OS. Useful when
    /// additional short spins are unlikely to help.
    #[inline]
    pub(crate) fn yielder(&self) {
        self.step.set(self.spin_limit + 1);
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
        let spins = self.step.get();
        if spins <= self.spin_limit {
            for _ in 0..1 << spins {
                hint::spin_loop();
            }
        } else {
            thread::yield_now();
        }

        if spins <= self.yield_limit {
            self.step.set(spins + 1);
        }
    }

    /// Returns `true` once backoff has exceeded `YIELD_LIMIT` and blocking is advised.
    ///
    /// At this point, prefer parking the thread or using a different synchronization
    /// mechanism instead of continuing to spin/yield.
    #[inline]
    pub(crate) fn is_completed(&self) -> bool {
        self.step.get() > self.yield_limit
    }

    /// Returns `true` once the backoff has reached the yielding phase (`step >= SPIN_LIMIT`).
    ///
    /// When this is `true`, subsequent calls to [`snooze`] will yield the current
    /// thread to the OS scheduler.
    #[inline]
    pub(crate) fn is_yielding(&self) -> bool {
        self.step.get() >= self.spin_limit
    }

    #[inline]
    pub(crate) fn cpu_relax(mut counter: u32) {
        if counter >= 10 {
            return;
        }

        counter += 1;

        if counter <= 3 {
            for _ in 0..(1 << counter) {
                hint::spin_loop();
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
