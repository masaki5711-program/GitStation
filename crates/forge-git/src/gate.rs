//! A per-key concurrency cap with a short, bounded wait (load test
//! 2026-11-17 F1 / F4): at most `max` holders per key; more callers wait
//! in a fair (FIFO) queue of at most `max_waiters`, each for at most the
//! wait it asks for. A full queue is refused at once, a wait that runs out
//! after it; the caller turns either into its own `FG-GIT-0004` (with
//! [`Refusal::into_error`]). Entries are removed when nobody holds or
//! waits, so the map is bounded by the keys in use.
//!
//! Used by the per-repository browse cap (`browse`), the per-principal
//! stream cap (`stream`) and forge-web's per-principal body reads.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use forge_errors::{ErrorCode, ForgeError};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::RETRY_AFTER_CONTEXT;

/// The cap (cheap to clone: shared state).
#[derive(Debug)]
pub struct KeyedGate<K> {
    map: Arc<Mutex<HashMap<K, Entry>>>,
    max: usize,
    max_waiters: usize,
}

impl<K> Clone for KeyedGate<K> {
    fn clone(&self) -> Self {
        Self {
            map: Arc::clone(&self.map),
            max: self.max,
            max_waiters: self.max_waiters,
        }
    }
}

#[derive(Debug)]
struct Entry {
    slots: Arc<Semaphore>,
    /// Holders and waiters.
    users: usize,
}

/// One held place; released on drop (the slot, then the place).
#[derive(Debug)]
pub struct GatePermit<K: Eq + Hash> {
    _permit: OwnedSemaphorePermit,
    _place: Place<K>,
}

/// A caller's place at a key (holding or waiting); given back on drop.
#[derive(Debug)]
struct Place<K: Eq + Hash> {
    map: Arc<Mutex<HashMap<K, Entry>>>,
    key: K,
}

impl<K: Eq + Hash> Drop for Place<K> {
    fn drop(&mut self) {
        let mut map = self.map.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = map.get_mut(&self.key) {
            entry.users = entry.users.saturating_sub(1);
            if entry.users == 0 {
                map.remove(&self.key);
            }
        }
    }
}

/// Why no place was given.
#[derive(Debug)]
pub enum Refusal {
    /// `max_waiters` already wait: refused at once.
    QueueFull,
    /// Waited this long without a free slot.
    Timeout(Duration),
    /// The semaphore was closed (a bug, never load).
    Closed(tokio::sync::AcquireError),
}

impl Refusal {
    /// `busy` (the caller's `FG-GIT-0004` with its `reason`) with `queue`
    /// (`full` / `timeout`), `waited_ms`, `max_waiters` and the retry hint
    /// [`RETRY_AFTER_CONTEXT`] (1 s); a closed semaphore is `FG-SYS-0001`.
    pub fn into_error(self, busy: ForgeError, max_waiters: usize) -> ForgeError {
        let (queue, waited) = match self {
            Refusal::QueueFull => ("full", Duration::ZERO),
            Refusal::Timeout(waited) => ("timeout", waited),
            Refusal::Closed(e) => {
                return ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "gate_closed")
                    .with_source(e)
            }
        };
        busy.with_context("queue", queue)
            .with_context("waited_ms", waited.as_millis())
            .with_context("max_waiters", max_waiters)
            .with_context(RETRY_AFTER_CONTEXT, 1)
    }
}

impl<K: Eq + Hash + Clone> KeyedGate<K> {
    /// `max` holders per key (at least 1), `max_waiters` waiting per key.
    pub fn new(max: usize, max_waiters: usize) -> Self {
        Self {
            map: Arc::new(Mutex::new(HashMap::new())),
            max: max.max(1),
            max_waiters,
        }
    }

    pub fn max(&self) -> usize {
        self.max
    }

    pub fn max_waiters(&self) -> usize {
        self.max_waiters
    }

    /// A place for `key`: at once when a slot is free, else after waiting
    /// at most `wait` (zero: refused at once when none is free).
    pub async fn acquire(&self, key: &K, wait: Duration) -> Result<GatePermit<K>, Refusal> {
        let (slots, place) = {
            let mut map = self.map.lock().unwrap_or_else(PoisonError::into_inner);
            let entry = map.entry(key.clone()).or_insert_with(|| Entry {
                slots: Arc::new(Semaphore::new(self.max)),
                users: 0,
            });
            if entry.users >= self.max.saturating_add(self.max_waiters) {
                return Err(Refusal::QueueFull);
            }
            entry.users += 1;
            (
                Arc::clone(&entry.slots),
                Place {
                    map: Arc::clone(&self.map),
                    key: key.clone(),
                },
            )
        };
        // From here `place` is given back on every path. `timeout` polls
        // the acquire first: a free slot is taken even with a zero wait.
        match tokio::time::timeout(wait, slots.acquire_owned()).await {
            Ok(Ok(permit)) => Ok(GatePermit {
                _permit: permit,
                _place: place,
            }),
            Ok(Err(e)) => Err(Refusal::Closed(e)),
            Err(_) => Err(Refusal::Timeout(wait)),
        }
    }

    /// Slots of `key` held now.
    pub fn held(&self, key: &K) -> usize {
        let map = self.map.lock().unwrap_or_else(PoisonError::into_inner);
        map.get(key)
            .map_or(0, |e| self.max.saturating_sub(e.slots.available_permits()))
    }

    /// Callers holding or waiting for `key`.
    pub fn users(&self, key: &K) -> usize {
        let map = self.map.lock().unwrap_or_else(PoisonError::into_inner);
        map.get(key).map_or(0, |e| e.users)
    }
}

#[cfg(test)]
#[path = "gate_tests.rs"]
mod tests;
