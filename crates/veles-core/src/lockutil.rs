//! Lock helpers — poison recovery for the gateway's shared std Mutexes.
//!
//! The Aug 22 desktop-API incident: one panic while a lock was held
//! (a `blocking_lock` inside an async context) poisoned the store
//! mutex; every later `lock().unwrap()` on any thread then panicked
//! with PoisonError, taking down request after request until the whole
//! gateway was restarted. A poisoned mutex still hands out the guard —
//! the data is only *possibly* inconsistent. For veles' usage (store =
//! SQLite with its own transactions, agent maps = plain HashMaps) the
//! panic almost never left broken invariants, and even when it did,
//! crashing every subsequent caller is strictly worse than limping on.
//!
//! Use `lock_ok(&mutex)` everywhere a `lock().unwrap()` used to be.

use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Extension trait: poison-recovering lock on std Mutexes.
pub trait LockOk<T> {
    /// Lock, recovering from poison instead of panicking.
    fn lock_ok(&self) -> MutexGuard<'_, T>;
    /// Try to lock without blocking; None if contended. Poison-recovering.
    fn try_lock_ok(&self) -> Option<MutexGuard<'_, T>>;
}

impl<T> LockOk<T> for Mutex<T> {
    fn lock_ok(&self) -> MutexGuard<'_, T> {
        match self.lock() {
            Ok(g) => g,
            Err(poisoned) => {
                tracing::warn!("poisoned mutex recovered (previous holder panicked) — continuing");
                poisoned.into_inner()
            }
        }
    }

    fn try_lock_ok(&self) -> Option<MutexGuard<'_, T>> {
        match self.try_lock() {
            Ok(g) => Some(g),
            Err(std::sync::TryLockError::WouldBlock) => None,
            Err(std::sync::TryLockError::Poisoned(p)) => {
                tracing::warn!("poisoned mutex recovered (try_lock path)");
                Some(p.into_inner())
            }
        }
    }
}

/// Same recovery for RwLock: a reader/writer that panicked mid-critical
/// section must not take every later caller down with it.
pub trait RwLockOk<T> {
    fn read_ok(&self) -> RwLockReadGuard<'_, T>;
    fn write_ok(&self) -> RwLockWriteGuard<'_, T>;
}

impl<T> RwLockOk<T> for RwLock<T> {
    fn read_ok(&self) -> RwLockReadGuard<'_, T> {
        match self.read() {
            Ok(g) => g,
            Err(poisoned) => {
                tracing::warn!("poisoned RwLock recovered (read) — continuing");
                poisoned.into_inner()
            }
        }
    }

    fn write_ok(&self) -> RwLockWriteGuard<'_, T> {
        match self.write() {
            Ok(g) => g,
            Err(poisoned) => {
                tracing::warn!("poisoned RwLock recovered (write) — continuing");
                poisoned.into_inner()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_from_poison() {
        let m: Mutex<u32> = Mutex::new(7);
        let _ = m.lock().map(|g| *g);
        // Poison it by panicking while held.
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = m.lock().unwrap();
            panic!("deliberate");
        }));
        assert!(r.is_err());
        // lock_ok must NOT panic and must hand back the inner value.
        assert_eq!(*m.lock_ok(), 7);
    }

    #[test]
    fn try_lock_ok_recovers_and_blocks_correctly() {
        let m: Mutex<Vec<String>> = Mutex::new(Vec::new());
        {
            let mut g = m.try_lock_ok().expect("uncontended lock succeeds");
            g.push("x".into());
        }
        assert_eq!(m.try_lock_ok().map(|g| g.len()), Some(1));
    }

    #[test]
    fn rwlock_recovers_from_poison() {
        let l: RwLock<u32> = RwLock::new(11);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = l.write().unwrap();
            panic!("deliberate");
        }));
        assert!(r.is_err());
        assert_eq!(*l.read_ok(), 11);
        *l.write_ok() = 12;
        assert_eq!(*l.read_ok(), 12);
    }
}
