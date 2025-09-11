use crate::core::scondvar::SCondVar;
use crate::core::smutex::{SGuard, SMutex};
use std::cell::UnsafeCell;
use std::fmt;
use std::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed, Release};
use std::sync::atomic::{fence, AtomicUsize};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

pub(crate) enum GmutexType {
    Exclusive,
    Group,
}

type State = usize;

const UNLOCKED: State = 0;
const LOCKED_EXCLUSIVE: State = 1;
const LOCKED_GROUP: State = 2;
const DIRTY: State = 3;

/// passo per codificare uno stato specifico di gruppo: LOCKED_GROUP + GROUP_STEP * n
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
    // stato canonico (usato quando si tiene il Gmutex)
    state: State,
    // numero di thread attualmente 'in attesa' (come nella logica originale)
    wakers: usize,
    // contatori per tipo di gruppo; slot per tipo n è counts[n]
    counts: Vec<usize>,
    // Nota: non teniamo qui total_g_locked (è atomico nella struttura principale)
}

struct InnerGmutex {
    state_gmutex: SMutex,
    data: UnsafeCell<StateData>,

    // condvar globale per notifiche su tutti i gruppi (usata quando serve "tutti")
    cvar_group: SCondVar,
    // condvars per singolo tipo di gruppo (Arc per sicurezza durante wait fuori dal lock)
    group_cvars: UnsafeCell<Vec<Option<Arc<SCondVar>>>>,

    cvar_exclusive: SCondVar,
    ref_count: AtomicUsize,

    // primitive atomiche per letture hot-path:
    // - state_atomic riflette lo stesso valore di `state` (ma può essere letto senza Gmutex)
    // - total_g_locked_atomic tiene il conteggio totale dei lockers di gruppo (somma su counts)
    state_atomic: AtomicUsize,
    total_g_locked_atomic: AtomicUsize,
}

unsafe impl Send for InnerGmutex {}
unsafe impl Sync for InnerGmutex {}

impl InnerGmutex {
    fn new() -> Self {
        Self {
            state_gmutex: SMutex::new(),
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
    /// Must be called under state_gmutex lock.
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

    /// get or create a Arc<SCondVar> for group n — must hold Gmutex
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

    /// notify_all over all group condvars; must be called under Gmutex.
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
pub struct Gmutex {
    ptr: *const InnerGmutex,
}

unsafe impl Send for Gmutex {}
unsafe impl Sync for Gmutex {}

impl std::panic::UnwindSafe for Gmutex {}
impl std::panic::RefUnwindSafe for Gmutex {}

impl Gmutex {
    pub fn new() -> Self {
        let ptr = Box::into_raw(Box::new(InnerGmutex::new()));
        if ptr.is_null() {
            panic!("Happened an invalid allocation for Gmutex");
        }
        Self { ptr }
    }

    #[inline]
    fn inner(&self) -> &InnerGmutex {
        unsafe { &*self.ptr }
    }

    pub fn get_ref_count(&self) -> usize {
        self.inner().ref_count.load(Acquire)
    }

    /// ritorna il conteggio totale di locker di gruppo — O(1) read atomica, lock-free
    pub fn get_group_locked(&self) -> usize {
        self.inner().total_g_locked_atomic.load(Acquire)
    }

    /// ritorna il conteggio di locker per uno specifico tipo `n`
    /// questa richiede il lock (perché i contatori per tipo sono nel Vec)
    pub fn get_group_locked_for(&self, n: usize) -> usize {
        let guard = self.inner().state_gmutex.lock();
        let d = self.inner().data(&guard);
        if n < d.counts.len() { d.counts[n] } else { 0 }
    }

    /// se c'è qualunque locker di gruppo (qualsiasi tipo) — lettura atomica veloce
    pub fn is_locked_group(&self) -> bool {
        let st = self.inner().state_atomic.load(Acquire);
        let total = self.inner().total_g_locked_atomic.load(Acquire);
        st == LOCKED_GROUP || (st == DIRTY && total > 0) || decode_group_state(st).is_some()
    }

    /// se è locked esclusivo — lettura atomica
    pub fn is_locked_exclusive(&self) -> bool {
        self.inner().state_atomic.load(Acquire) == LOCKED_EXCLUSIVE
    }

    /// se è locked (esclusivo o gruppi) — lettura atomica
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
        let mut guard = self.inner().state_gmutex.lock();

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

    /// lock di gruppo per il tipo `n`
    pub fn lock_group(&self, n: usize) {
        let mut guard = self.inner().state_gmutex.lock();

        // ensure capacity (single resize path under Gmutex)
        self.inner().ensure_group_capacity(n);

        {
            let d = self.inner().data(&guard);

            d.wakers = d.wakers.saturating_add(1);

            // incrementa contatore per quel tipo (O(1, no hash)
            d.counts[n] = d.counts[n].saturating_add(1);

            // incrementa il contatore totale atomico (solo qui, sotto Gmutex)
            self.inner().total_g_locked_atomic.fetch_add(1, AcqRel);
        }

        // prendi un Arc<SCondVar> stabile per questo gruppo mentre siamo sotto lock
        let gcv_arc = self.inner().get_or_create_group_cvar_arc(n);

        loop {
            let mut done = false;

            {
                let d = self.inner().data(&guard);

                // consentiamo il lock di gruppo quando non è presente LOCKED_EXCLUSIVE
                if d.state != LOCKED_EXCLUSIVE {
                    // aggiorna lo stato: se ora c'è un solo tipo di gruppo lockato -> impostalo
                    // compute number of non-zero slots cheaply: we can check total atomics and counts.len()
                    // but simplest: check how many slots are non-zero by scanning counts if counts.len is small.
                    // To avoid O(k) here, use heuristic: if total==d.counts[n], then only this group has locks.
                    let total_now = self.inner().total_g_locked_atomic.load(Acquire);
                    if total_now == d.counts[n] {
                        d.state = group_state(n);
                    } else {
                        d.state = LOCKED_GROUP;
                    }

                    // manteniamo coerente lo stato atomico
                    self.inner().state_atomic.store(d.state, Release);

                    // Notifica in modo mirato: preferisco notificare la condvar del gruppo
                    gcv_arc.notify_all();

                    d.wakers = d.wakers.saturating_sub(1);
                    done = true;
                }
            }

            if done {
                break;
            }

            // adesso il borrow di d è terminato, quindi possiamo riassegnare guard
            guard = gcv_arc.wait(guard);
        }
    }

    /// unlock di gruppo per il tipo `n`
    pub fn unlock_group(&self, n: usize) {
        let guard = self.inner().state_gmutex.lock();
        let d = self.inner().data(&guard);

        if d.state != LOCKED_GROUP && decode_group_state(d.state).is_none() && d.state != DIRTY {
            panic!("Trying to unlock a non Locked Group {}", d.state);
        }

        // decrementa wakers (come prima)
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
        // aggiorna contatore totale atomico
        self.inner().total_g_locked_atomic.fetch_sub(1, AcqRel);

        if d.counts[n] == 0 {
            // keep slot zero, do not shrink vectors
        }

        // aggiorna lo stato in base al numero di gruppi rimanenti
        let total_after = self.inner().total_g_locked_atomic.load(Acquire);
        if total_after == 0 {
            d.state = DIRTY;
        } else {
            // se solo un gruppo rimane con contatori >0, vogliamo settare group_state(only_n)
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

        // aggiorna lo stato atomico
        self.inner().state_atomic.store(d.state, Release);

        // risveglia: se non ci sono più lockers di qualsiasi tipo, risvegliamo l'exclusive;
        // inoltre notifichiamo il gruppo appena sbloccato e la condvar globale
        self.inner().cvar_exclusive.notify_one();
        if let Some(gcv_arc) = self.inner().get_group_cvar_if_exists_arc(n) {
            gcv_arc.notify_all();
        }
        self.inner().cvar_group.notify_all();
    }

    /// unlock exclusive
    pub fn unlock_exclusive(&self) {
        let guard = self.inner().state_gmutex.lock();
        let d = self.inner().data(&guard);

        if d.state != LOCKED_EXCLUSIVE {
            panic!("Is not Locked Exclusive (state = {})", d.state);
        }

        // aggiorna stato in base ai gruppi presenti (usando total_g_locked_atomic O(1))
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

        // aggiorna l'atomico dopo la modifica canonica sotto Gmutex
        self.inner().state_atomic.store(d.state, Release);

        d.wakers = d.wakers.saturating_sub(1);

        // Notifica mirata: preferisco notificare i gruppi esistenti invece di tutto il mondo
        self.inner().notify_all_group_cvars();
        self.inner().cvar_group.notify_all();
        self.inner().cvar_exclusive.notify_one();
    }

    /// azzera tutti i locker di gruppo o solo il tipo passato (Some(n)).
    /// - Some(n): azzera solo il gruppo n
    /// - None: azzera tutti i gruppi (comportamento 'tutti')
    pub fn unlock_all_group(&self, target: Option<usize>) {
        let guard = self.inner().state_gmutex.lock();
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

        // aggiorna atomico
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

    /// sospende un thread in attesa per Exclusive o Group.
    /// Se `group_id` è Some(n), aspetta sulla condvar del gruppo n; altrimenti sulla condvar globale.
    pub(crate) fn suspend(&self, t: GmutexType, group_id: Option<usize>) -> bool {
        let guard = self.inner().state_gmutex.lock();
        let d = self.inner().data(&guard);
        if d.wakers == 0 {
            return false;
        }

        match t {
            GmutexType::Exclusive => {
                let _ = self.inner().cvar_exclusive.wait(guard);
            }
            GmutexType::Group => {
                match group_id {
                    Some(n) => {
                        // prendi Arc qui (sotto lock) e poi aspetta su di essa
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
    pub(crate) fn wake_all(&self, t: GmutexType, group_id: Option<usize>) {
        match t {
            GmutexType::Exclusive => self.inner().cvar_exclusive.notify_all(),
            GmutexType::Group => {
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
    pub(crate) fn wake(&self, t: GmutexType, group_id: Option<usize>) -> bool {
        match t {
            GmutexType::Exclusive => {
                self.inner().cvar_exclusive.notify_one();
                true
            }
            GmutexType::Group => match group_id {
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

impl Clone for Gmutex {
    fn clone(&self) -> Self {
        self.inner().ref_count.fetch_add(1, Relaxed);
        Gmutex { ptr: self.ptr }
    }
}

impl Drop for Gmutex {
    fn drop(&mut self) {
        if self.inner().ref_count.fetch_sub(1, AcqRel) == 1 {
            fence(Acquire);
            let ptr = self.ptr as *mut InnerGmutex;
            unsafe { drop(Box::from_raw(ptr)) };
        }
    }
}

impl fmt::Debug for Gmutex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // per Debug siamo ok ad acquisire il Gmutex (operazione infrequente)
        let guard = self.inner().state_gmutex.lock();
        let d = self.inner().data(&guard);
        let total = self.inner().total_g_locked_atomic.load(Acquire);
        f.debug_struct("Gmutex")
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
