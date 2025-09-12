use crate::core::scondvar::SCondVar;
use crate::core::smutex::{SGuard, SMutex};
use std::cell::UnsafeCell;
use std::fmt;
use std::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed, Release};
use std::sync::atomic::{fence, AtomicUsize};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

pub(crate) enum GrutexType {
    Exclusive,
    Group,
}

type State = usize;

const UNLOCKED: State = 0;
const LOCKED_EXCLUSIVE: State = 1;
const LOCKED_GROUP: State = 2;
const DIRTY: State = 3;

/// Step to encode a specific group state: LOCKED_GROUP + GROUP_STEP * n
const GROUP_STEP: State = 5;

#[inline]
fn group_state(n: usize) -> State {
    LOCKED_GROUP + (GROUP_STEP * (n as State))
}

#[inline]
fn decode_group_state(st: State) -> Option<usize> {
    if st <= LOCKED_GROUP {
        return None;
    }
    let diff = st - LOCKED_GROUP;
    if diff % GROUP_STEP != 0 {
        return None;
    }
    Some((diff / GROUP_STEP) as usize)
}

struct StateData {
    // Canonical state (used while holding the Grutex)
    state: State,
    // Number of threads currently 'waiting' (as in the original logic)
    wakers: usize,
    // Per-group counters; the slot for group n is counts[n]
    counts: Vec<usize>,
    // Note: we don't keep total_g_locked here (it's atomic in the main struct)
}

struct InnerGrutex {
    state_grutex: SMutex,
    data: UnsafeCell<StateData>,

    // Global condvar for notifications across all groups (used for the "all" case)
    cvar_group: SCondVar,
    // Per-group condvars (Arc to be safe when waiting outside the lock)
    group_cvars: UnsafeCell<Vec<Option<Arc<SCondVar>>>>,

    cvar_exclusive: SCondVar,
    ref_count: AtomicUsize,

    // Atomic fields for hot-path reads:
    // - state_atomic mirrors the `state` value (can be read without the Grutex)
    // - total_g_locked_atomic holds the total number of group lockers (sum of counts)
    state_atomic: AtomicUsize,
    total_g_locked_atomic: AtomicUsize,
}

unsafe impl Send for InnerGrutex {}
unsafe impl Sync for InnerGrutex {}

impl InnerGrutex {
    fn new() -> Self {
        Self {
            state_grutex: SMutex::new(),
            data: UnsafeCell::new(StateData {
                state: UNLOCKED,
                wakers: 0,
                counts: Vec::new(),
            }),
            cvar_group: SCondVar::new(),
            group_cvars: UnsafeCell::new(Vec::new()),
            cvar_exclusive: SCondVar::new(),
            ref_count: AtomicUsize::new(1),
            state_atomic: AtomicUsize::new(UNLOCKED),
            total_g_locked_atomic: AtomicUsize::new(0),
        }
    }

    #[inline]
    fn data<'a>(&'a self, _guard: &'a SGuard<'a>) -> &'a mut StateData {
        unsafe { &mut *self.data.get() }
    }

    /// Ensure vectors are large enough for index `n`.
    /// Must be called under state_grutex lock.
    #[inline]
    fn ensure_group_capacity(&self, n: usize) {
        let counts = unsafe { &mut (*self.data.get()).counts };
        if n >= counts.len() {
            // grow counts to accommodate n; double strategy
            let mut new_len = counts.len().max(1);
            while n >= new_len {
                new_len *= 2;
            }
            counts.resize(new_len, 0);
        }

        let vec = unsafe { &mut *self.group_cvars.get() };
        if n >= vec.len() {
            let mut new_len = vec.len().max(1);
            while n >= new_len {
                new_len *= 2;
            }
            vec.resize_with(new_len, || None);
        }
    }

    /// get or create an Arc<SCondVar> for group n — must hold Grutex
    #[inline]
    fn get_or_create_group_cvar_arc(&self, n: usize) -> Arc<SCondVar> {
        let vec = unsafe { &mut *self.group_cvars.get() };
        if n >= vec.len() {
            // caller should have ensured capacity; but be defensive
            let mut new_len = vec.len().max(1);
            while n >= new_len {
                new_len *= 2;
            }
            vec.resize_with(new_len, || None);
        }
        match &vec[n] {
            Some(a) => a.clone(),
            None => {
                let a = Arc::new(SCondVar::new());
                vec[n] = Some(a.clone());
                a
            }
        }
    }

    /// clone Arc if exists
    #[inline]
    fn get_group_cvar_if_exists_arc(&self, n: usize) -> Option<Arc<SCondVar>> {
        let vec = unsafe { &mut *self.group_cvars.get() };
        if n >= vec.len() {
            None
        } else {
            vec[n].as_ref().cloned()
        }
    }

    /// notify_all over all group condvars; must be called under Grutex.
    #[inline]
    fn notify_all_group_cvars(&self) {
        let vec = unsafe { &mut *self.group_cvars.get() };
        for opt in vec.iter() {
            if let Some(cv) = opt {
                cv.notify_all();
            }
        }
    }
}

#[repr(transparent)]
pub struct Grutex {
    ptr: *const InnerGrutex,
}

unsafe impl Send for Grutex {}
unsafe impl Sync for Grutex {}

impl std::panic::UnwindSafe for Grutex {}
impl std::panic::RefUnwindSafe for Grutex {}

impl Grutex {
    pub fn new() -> Self {
        let ptr = Box::into_raw(Box::new(InnerGrutex::new()));
        if ptr.is_null() {
            panic!("Happened an invalid allocation for Grutex");
        }
        Self { ptr }
    }

    #[inline]
    fn inner(&self) -> &InnerGrutex {
        unsafe { &*self.ptr }
    }

    pub fn get_ref_count(&self) -> usize {
        self.inner().ref_count.load(Acquire)
    }

    /// Returns the total number of group lockers — O(1) atomic read, lock-free
    pub fn get_group_locked(&self) -> usize {
        self.inner().total_g_locked_atomic.load(Acquire)
    }

    /// Returns the number of lockers for a specific group `n`
    /// Requires the lock (per-group counters live in a Vec)
    pub fn get_group_locked_for(&self, n: usize) -> usize {
        let guard = self.inner().state_grutex.lock();
        let d = self.inner().data(&guard);
        if n < d.counts.len() { d.counts[n] } else { 0 }
    }

    /// Whether there is any group locker (any type) — fast atomic read
    pub fn is_locked_group(&self) -> bool {
        let st = self.inner().state_atomic.load(Acquire);
        let total = self.inner().total_g_locked_atomic.load(Acquire);
        st == LOCKED_GROUP || (st == DIRTY && total > 0) || decode_group_state(st).is_some()
    }

    /// Whether it is exclusively locked — atomic read
    pub fn is_locked_exclusive(&self) -> bool {
        self.inner().state_atomic.load(Acquire) == LOCKED_EXCLUSIVE
    }

    /// Whether it is locked (exclusive or groups) — atomic read
    pub fn is_locked(&self) -> bool {
        let st = self.inner().state_atomic.load(Acquire);
        let total = self.inner().total_g_locked_atomic.load(Acquire);
        st == LOCKED_EXCLUSIVE
            || st == LOCKED_GROUP
            || (st == DIRTY && total > 0)
            || decode_group_state(st).is_some()
    }

    #[inline]
    fn spin(&self, _spin: usize) -> State {
        self.inner().state_atomic.load(Acquire)
    }

    pub fn lock_exclusive(&self) {
        let mut guard = self.inner().state_grutex.lock();

        loop {
            {
                let d = self.inner().data(&guard);
                d.wakers = d.wakers.saturating_add(1);

                let total_locked = self.inner().total_g_locked_atomic.load(Acquire);
                if d.state == UNLOCKED || (d.state == DIRTY && total_locked == 0) {
                    d.state = LOCKED_EXCLUSIVE;
                    self.inner().state_atomic.store(LOCKED_EXCLUSIVE, Release);
                    d.wakers = d.wakers.saturating_sub(1);
                    break;
                }
            }

            guard = self.inner().cvar_exclusive.wait(guard);
        }
    }

    /// Group lock for type `n`
    pub fn lock_group(&self, n: usize) {
        let mut guard = self.inner().state_grutex.lock();

        // ensure capacity (single resize path under Grutex)
        self.inner().ensure_group_capacity(n);

        {
            let d = self.inner().data(&guard);

            d.wakers = d.wakers.saturating_add(1);

            // Increment counter for that type (O(1), no hash)
            d.counts[n] = d.counts[n].saturating_add(1);

            // Increment the total atomic counter (only here, under the Grutex)
            self.inner().total_g_locked_atomic.fetch_add(1, AcqRel);
        }

        // Grab a stable Arc<SCondVar> for this group while under the lock
        let gcv_arc = self.inner().get_or_create_group_cvar_arc(n);

        loop {
            let mut done = false;

            {
                let d = self.inner().data(&guard);

                // Allow group locking when LOCKED_EXCLUSIVE is not present
                if d.state != LOCKED_EXCLUSIVE {
                    // Update state: if only one group type is locked now -> set specific state
                    // compute number of non-zero slots cheaply: we can check total atomics and counts.len()
                    // but simplest: check how many slots are non-zero by scanning counts if counts.len is small.
                    // To avoid O(k) here, use heuristic: if total==d.counts[n], then only this group has locks.
                    let total_now = self.inner().total_g_locked_atomic.load(Acquire);
                    if total_now == d.counts[n] {
                        d.state = group_state(n);
                    } else {
                        d.state = LOCKED_GROUP;
                    }

                    // Keep the atomic state in sync
                    self.inner().state_atomic.store(d.state, Release);

                    // Targeted notify: prefer notifying the group's condvar
                    gcv_arc.notify_all();

                    d.wakers = d.wakers.saturating_sub(1);
                    done = true;
                }
            }

            if done {
                break;
            }

            // The borrow of `d` has ended, so we can reassign `guard`
            guard = gcv_arc.wait(guard);
        }
    }

    /// Group unlock for type `n`
    pub fn unlock_group(&self, n: usize) {
        let guard = self.inner().state_grutex.lock();
        let d = self.inner().data(&guard);

        if d.state != LOCKED_GROUP && decode_group_state(d.state).is_none() && d.state != DIRTY {
            panic!("Trying to unlock a non Locked Group {}", d.state);
        }

        // Decrement wakers (as before)
        d.wakers = d.wakers.saturating_sub(1);

        if n >= d.counts.len() {
            panic!(
                "Trying to unlock group type {} which was not locked (out of bounds)",
                n
            );
        }

        let prev = d.counts[n];
        if prev == 0 {
            panic!("Trying to unlock group type {} which had zero lockers", n);
        }
        d.counts[n] = prev - 1;
        // Update the total atomic counter
        self.inner().total_g_locked_atomic.fetch_sub(1, AcqRel);

        if d.counts[n] == 0 {
            // keep slot zero, do not shrink vectors
        }

        // Update state based on the number of groups remaining
        let total_after = self.inner().total_g_locked_atomic.load(Acquire);
        if total_after == 0 {
            d.state = DIRTY;
        } else {
            // If only one group remains with count > 0, set group_state(only_n)
            // fast path: if d.counts[n] == total_after then other groups are zero
            // but n may be not the only one; we find any single non-zero slot if needed.
            // attempt fast detection: if total_after == d.counts.iter().sum() (but sum is O(k))
            // cheaper heuristic: if total_after == d.counts[n] { only_n_or_other? }
            // we'll check: if total_after == d.counts[n] then only this group has locks
            // else: LOCKED_GROUP
            let mut set_specific = None;
            if total_after > 0 {
                // fast check: try to find a single non-zero slot without full sum if possible
                let mut non_zero = 0usize;
                let mut found_idx = None;
                for (idx, &c) in d.counts.iter().enumerate() {
                    if c != 0 {
                        non_zero += 1;
                        if found_idx.is_none() {
                            found_idx = Some(idx)
                        }
                        if non_zero > 1 {
                            break;
                        }
                    }
                }
                if non_zero == 1 {
                    set_specific = found_idx;
                }
            }

            if let Some(only_n) = set_specific {
                d.state = group_state(only_n);
            } else {
                d.state = LOCKED_GROUP;
            }
        }

        // Update the atomic state
        self.inner().state_atomic.store(d.state, Release);

        // Wakeups: if there are no lockers of any kind, wake the exclusive waiter;
        // also notify the just-unlocked group and the global condvar
        self.inner().cvar_exclusive.notify_one();
        if let Some(gcv_arc) = self.inner().get_group_cvar_if_exists_arc(n) {
            gcv_arc.notify_all();
        }
        self.inner().cvar_group.notify_all();
    }

    /// unlock exclusive
    pub fn unlock_exclusive(&self) {
        let guard = self.inner().state_grutex.lock();
        let d = self.inner().data(&guard);

        if d.state != LOCKED_EXCLUSIVE {
            panic!("Is not Locked Exclusive (state = {})", d.state);
        }

        // Update state based on present groups (using total_g_locked_atomic, O(1))
        let total = self.inner().total_g_locked_atomic.load(Acquire);
        if total == 0 {
            d.state = UNLOCKED;
        } else {
            // find single non-zero slot if exists
            let mut non_zero = 0usize;
            let mut only_idx = None;
            for (i, &c) in d.counts.iter().enumerate() {
                if c != 0 {
                    non_zero += 1;
                    only_idx = Some(i);
                    if non_zero > 1 {
                        break;
                    }
                }
            }
            if non_zero == 1 {
                d.state = group_state(only_idx.unwrap());
            } else {
                d.state = LOCKED_GROUP;
            }
        }

        // Update the atomic after the canonical change under the Grutex
        self.inner().state_atomic.store(d.state, Release);

        d.wakers = d.wakers.saturating_sub(1);

        // Targeted notifications: prefer notifying existing groups instead of everyone
        self.inner().notify_all_group_cvars();
        self.inner().cvar_group.notify_all();
        self.inner().cvar_exclusive.notify_one();
    }

    /// Reset all group lockers or only the specified type (Some(n)).
    /// - Some(n): reset only group n
    /// - None: reset all groups (the 'all' behavior)
    pub fn unlock_all_group(&self, target: Option<usize>) {
        let guard = self.inner().state_grutex.lock();
        let d = self.inner().data(&guard);

        if d.state != LOCKED_GROUP && decode_group_state(d.state).is_none() && d.state != DIRTY {
            panic!("Trying to unlock a non Locked Group {}", d.state);
        }

        match target {
            Some(n) => {
                if n < d.counts.len() {
                    let prev = d.counts[n];
                    if prev > 0 {
                        d.counts[n] = 0;
                        self.inner().total_g_locked_atomic.fetch_sub(prev, AcqRel);
                        d.wakers = d.wakers.saturating_sub(prev);
                    }
                }
            }
            None => {
                let prev_total = self.inner().total_g_locked_atomic.load(Acquire);
                if prev_total > 0 {
                    for slot in d.counts.iter_mut() {
                        *slot = 0;
                    }
                    self.inner().total_g_locked_atomic.store(0, Release);
                    d.wakers = d.wakers.saturating_sub(prev_total);
                }
            }
        }

        // aggiorna stato: se nessun gruppo rimane -> DIRTY; se 1 rimane -> state specifico; altrimenti LOCKED_GROUP
        let total_now = self.inner().total_g_locked_atomic.load(Acquire);
        if total_now == 0 {
            d.state = DIRTY;
        } else {
            // find single non-zero slot if exists
            let mut non_zero = 0usize;
            let mut only_idx = None;
            for (i, &c) in d.counts.iter().enumerate() {
                if c != 0 {
                    non_zero += 1;
                    only_idx = Some(i);
                    if non_zero > 1 {
                        break;
                    }
                }
            }
            if non_zero == 1 {
                d.state = group_state(only_idx.unwrap());
            } else {
                d.state = LOCKED_GROUP;
            }
        }

        // Update the atomic
        self.inner().state_atomic.store(d.state, Release);

        // notify appropriate condvars
        match target {
            Some(n) => {
                if let Some(gcv_arc) = self.inner().get_group_cvar_if_exists_arc(n) {
                    gcv_arc.notify_all();
                }
            }
            None => {
                self.inner().notify_all_group_cvars();
                self.inner().cvar_group.notify_all();
            }
        }
        self.inner().cvar_exclusive.notify_one();
    }

    /// Suspend a thread waiting for Exclusive or Group.
    /// If `group_id` is Some(n), wait on group n's condvar; otherwise on the global condvar.
    pub(crate) fn suspend(&self, t: GrutexType, group_id: Option<usize>) -> bool {
        let guard = self.inner().state_grutex.lock();
        let d = self.inner().data(&guard);
        if d.wakers == 0 {
            return false;
        }

        match t {
            GrutexType::Exclusive => {
                let _ = self.inner().cvar_exclusive.wait(guard);
            }
            GrutexType::Group => {
                match group_id {
                    Some(n) => {
                        // Take the Arc here (under the lock) then wait on it
                        if let Some(gcv_arc) = self.inner().get_group_cvar_if_exists_arc(n) {
                            let _ = gcv_arc.wait(guard);
                        } else {
                            let _ = self.inner().cvar_group.wait(guard);
                        }
                    }
                    None => {
                        let _ = self.inner().cvar_group.wait(guard);
                    }
                }
            }
        }

        true
    }

    pub(crate) fn pause(&self, timeout: Duration) {
        thread::sleep(timeout)
    }

    /// wake_all: se Some(n) => risveglia solo la condvar del gruppo n, altrimenti tutte le condvar di gruppo
    pub(crate) fn wake_all(&self, t: GrutexType, group_id: Option<usize>) {
        match t {
            GrutexType::Exclusive => self.inner().cvar_exclusive.notify_all(),
            GrutexType::Group => {
                match group_id {
                    Some(n) => {
                        if let Some(gcv_arc) = self.inner().get_group_cvar_if_exists_arc(n) {
                            gcv_arc.notify_all();
                        } else {
                            // se non esiste, fallback globale
                            self.inner().cvar_group.notify_all();
                        }
                    }
                    None => {
                        self.inner().notify_all_group_cvars();
                        self.inner().cvar_group.notify_all();
                    }
                }
            }
        }
    }

    /// wake: se Some(n) => notify_one sul gruppo n, altrimenti notify_one sulla condvar globale o su una per-group
    pub(crate) fn wake(&self, t: GrutexType, group_id: Option<usize>) -> bool {
        match t {
            GrutexType::Exclusive => {
                self.inner().cvar_exclusive.notify_one();
                true
            }
            GrutexType::Group => match group_id {
                Some(n) => {
                    if let Some(gcv_arc) = self.inner().get_group_cvar_if_exists_arc(n) {
                        gcv_arc.notify_one();
                    } else {
                        self.inner().cvar_group.notify_one();
                    }
                    true
                }
                None => {
                    let vec = unsafe { &*self.inner().group_cvars.get() };
                    if let Some(Some(cv)) = vec.iter().find(|o| o.is_some()) {
                        cv.notify_one();
                    } else {
                        self.inner().cvar_group.notify_one();
                    }
                    true
                }
            },
        }
    }
}

impl Clone for Grutex {
    fn clone(&self) -> Self {
        self.inner().ref_count.fetch_add(1, Relaxed);
        Grutex { ptr: self.ptr }
    }
}

impl Drop for Grutex {
    fn drop(&mut self) {
        if self.inner().ref_count.fetch_sub(1, Release) == 1 {
            fence(Acquire);
            let ptr = self.ptr as *mut InnerGrutex;
            unsafe { drop(Box::from_raw(ptr)) };
        }
    }
}

impl fmt::Debug for Grutex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // per Debug siamo ok ad acquisire il Grutex (operazione infrequente)
        let guard = self.inner().state_grutex.lock();
        let d = self.inner().data(&guard);
        let total = self.inner().total_g_locked_atomic.load(Acquire);
        f.debug_struct("Grutex")
            .field("state", &d.state)
            .field("exclusive", &(d.state == LOCKED_EXCLUSIVE))
            .field(
                "group",
                &(d.state == LOCKED_GROUP
                    || d.state == DIRTY
                    || decode_group_state(d.state).is_some()),
            )
            .field("lockers_total", &total)
            .field(
                "locker_types",
                &d.counts.iter().filter(|&&c| c != 0).count(),
            )
            .field("ref", &self.get_ref_count())
            .finish()
    }
}
