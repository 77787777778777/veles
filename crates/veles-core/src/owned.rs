//! Owner-tracked registration (PLAN-plugins-installer.md M4 / R2).
//!
//! A registry that can hold module code or module data stores each entry
//! as [`Owned<T>`]: the value plus the OWNER that registered it. The owner
//! is read from the registering thread's tag ([`crate::lifecycle::
//! thread_owner`]), NEVER passed in by the caller — so a module calling
//! core's ordinary API (`tools::register_global_tool`, `irc_tap::
//! register_sender`, …) gets tracked anyway, and existing core callers
//! register under `None` (core).
//!
//! The point is [`OwnedRegistry::purge`]: when a module reloads, the host
//! (R3) removes EXACTLY that module's entries and no one else's, so an IRC
//! module's tools vanish while a Telegram module's stay, and an entry a
//! module had overridden is in effect again. What outlives a registry
//! read must not hold module code: a session holds a module's tool
//! late-bound (`tools::session_tool`: the name, looked up per call), so a
//! re-registration after a reload is picked up, and the purge plus the
//! re-registration is the reload.
//!
//! What this module deliberately does NOT carry: the module-host's `Arc`
//! of the loaded library and the dispatch gate. Those belong to the R3
//! `Owned` wrapper the module host uses to call INTO module code; the
//! registry entry only needs to know WHO owns a value so it can be purged.
//! Keeping libloading and the gate out of core's registry keeps R2 free of
//! a libloading dependency and free of any behavior change on the gateway
//! today.

use std::hash::Hash;
use std::sync::RwLock;

/// A registered value plus the owner that registered it.
///
/// `owner` is `None` for anything a CORE thread registered (the gateway's
/// own boot wiring, builtin tools, config) — core is never purged. It is
/// `Some(tag)` for anything registered on a thread the host tagged with an
/// owner (a module instance's runtime thread), where `tag` is the owner
/// string like `"module:irc:synirc"`.
#[derive(Debug)]
pub struct Owned<T> {
    pub value: T,
    owner: Option<&'static str>,
}

impl<T> Owned<T> {
    /// Wrap `value`, attributing it to the CURRENT thread's owner. This is
    /// the only constructor registries should use; the owner is read from
    /// the thread tag so a module cannot lie about who it is.
    pub fn tracked(value: T) -> Self {
        Self {
            value,
            owner: crate::lifecycle::thread_owner(),
        }
    }

    /// Wrap `value` as core-owned, regardless of the calling thread. Used
    /// by boot wiring that runs on a module thread on behalf of core (none
    /// today; here so the contract has the explicit spelling).
    pub fn core(value: T) -> Self {
        Self { value, owner: None }
    }

    /// The registering owner, `None` for core.
    pub fn owner(&self) -> Option<&'static str> {
        self.owner
    }
}

/// A keyed registry of owner-tracked values, in registration order.
///
/// Two owners may hold the same key. The LATER registration is the one
/// lookups and iteration see; the earlier one is kept underneath it.
/// That is how a module overrides a core entry (M4: "everything can be
/// overridden"), and why [`purge`](Self::purge) can undo the override:
/// it removes the module's entries, and whatever they covered is visible
/// again. A re-registration by the SAME owner replaces its entry and
/// moves it to the end, which is what every registry here did before it
/// tracked owners (`retain` the name, then `push`), so the order the
/// registries iterate in (first hook verdict wins, first session runner
/// that owns a session) is the order things registered in.
pub struct OwnedRegistry<K, T> {
    entries: RwLock<Vec<(K, Owned<T>)>>,
}

impl<K, T> Default for OwnedRegistry<K, T> {
    fn default() -> Self {
        Self {
            entries: RwLock::new(Vec::new()),
        }
    }
}

impl<K, T> OwnedRegistry<K, T>
where
    K: Eq + Hash,
{
    /// Register `value` under `key` for the current thread's owner.
    pub fn insert(&self, key: K, value: T) {
        self.put(key, Owned::tracked(value));
    }

    /// Register `value` under `key` as CORE's, whatever the calling
    /// thread. For boot wiring that must not be purgeable.
    pub fn insert_core(&self, key: K, value: T) {
        self.put(key, Owned::core(value));
    }

    /// Register `value` under `key` for the current thread's owner, BELOW
    /// every other registration of `key`: an override point's default
    /// (PLAN-extension-api.md D2). A plugin that registered the name
    /// before the point was declared stays on top of it.
    pub fn insert_bottom(&self, key: K, value: T) {
        self.put_bottom(key, Owned::tracked(value));
    }

    /// [`insert_bottom`](Self::insert_bottom) as CORE's, whatever the
    /// calling thread: a point core declares lazily, maybe from the thread
    /// of the module whose registration triggered it.
    pub fn insert_core_bottom(&self, key: K, value: T) {
        self.put_bottom(key, Owned::core(value));
    }

    fn put_bottom(&self, key: K, item: Owned<T>) {
        let owner = item.owner();
        self.with_write(|v| {
            v.retain(|(k, e)| !(k == &key && e.owner() == owner));
            let at = v.iter().position(|(k, _)| k == &key).unwrap_or(v.len());
            v.insert(at, (key, item));
        });
    }

    /// Every registration of `key` with its owner, cloned, BOTTOM first:
    /// the last one answers, each covers the one before it.
    pub fn stack(&self, key: &K) -> Vec<(Option<&'static str>, T)>
    where
        T: Clone,
    {
        let v = self.read();
        v.iter()
            .filter(|(k, _)| k == key)
            .map(|(_, e)| (e.owner(), e.value.clone()))
            .collect()
    }

    fn put(&self, key: K, item: Owned<T>) {
        let owner = item.owner();
        self.with_write(|v| {
            v.retain(|(k, e)| !(k == &key && e.owner() == owner));
            v.push((key, item));
        });
    }

    /// Remove `key`: every owner's registration of it.
    pub fn remove(&self, key: &K) {
        self.with_write(|v| v.retain(|(k, _)| k != key));
    }

    /// Remove the entry `key` that `owner` registered, leaving other
    /// owners' entries for the same key in place (a module's override
    /// of a core tool goes; core's stays). True when one was removed.
    pub fn remove_owned(&self, key: &K, owner: Option<&'static str>) -> bool {
        let mut removed = false;
        self.with_write(|v| {
            let before = v.len();
            v.retain(|(k, e)| !(k == key && e.owner() == owner));
            removed = v.len() != before;
        });
        removed
    }

    /// Read the value `key` resolves to (its latest registration). The
    /// closure gets `&T`, so nothing escapes the lock.
    pub fn with<R>(&self, key: &K, f: impl FnOnce(&T) -> R) -> Option<R> {
        let v = self.read();
        v.iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, e)| f(&e.value))
    }

    /// The owner of the registration `key` resolves to.
    pub fn owner_of(&self, key: &K) -> Option<Option<&'static str>> {
        let v = self.read();
        v.iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, e)| e.owner())
    }

    /// Every owner holding `key`, oldest registration first: the last
    /// one answers, the others are covered by it.
    pub fn owners_of(&self, key: &K) -> Vec<Option<&'static str>> {
        let v = self.read();
        v.iter()
            .filter(|(k, _)| k == key)
            .map(|(_, e)| e.owner())
            .collect()
    }

    /// The registered keys, in registration order.
    pub fn keys(&self) -> Vec<K>
    where
        K: Clone,
    {
        let mut out = Vec::new();
        self.for_each(|k, _| out.push(k.clone()));
        out
    }

    /// The values the keys resolve to, cloned, in registration order.
    pub fn values_cloned(&self) -> Vec<T>
    where
        T: Clone,
    {
        let mut out = Vec::new();
        self.for_each(|_, e| out.push(e.value.clone()));
        out
    }

    /// Like [`values_cloned`](Self::values_cloned), with each value's
    /// owner.
    pub fn values_with_owners(&self) -> Vec<(T, Option<&'static str>)>
    where
        T: Clone,
    {
        let mut out = Vec::new();
        self.for_each(|_, e| out.push((e.value.clone(), e.owner())));
        out
    }

    /// Visit the entry each key resolves to, in registration order.
    /// Entries covered by a later registration of their key are skipped.
    pub fn for_each(&self, mut f: impl FnMut(&K, &Owned<T>)) {
        let v = self.read();
        for (i, (k, e)) in v.iter().enumerate() {
            if !v[i + 1..].iter().any(|(later, _)| later == k) {
                f(k, e);
            }
        }
    }

    /// Remove EXACTLY the entries `owner` registered, returning how many.
    /// What they covered is visible again.
    ///
    /// `purge(None)` removes nothing and returns 0: core is never purged.
    pub fn purge(&self, owner: Option<&'static str>) -> usize {
        let Some(owner) = owner else {
            return 0;
        };
        let mut removed = 0usize;
        self.with_write(|v| {
            let before = v.len();
            v.retain(|(_, e)| e.owner() != Some(owner));
            removed = before - v.len();
        });
        removed
    }

    /// The number of keys registered.
    pub fn len(&self) -> usize {
        let mut n = 0;
        self.for_each(|_, _| n += 1);
        n
    }

    pub fn is_empty(&self) -> bool {
        self.read().is_empty()
    }

    /// The read lock, recovered from poison: a panicking writer must not
    /// make every later read report "absent".
    fn read(&self) -> std::sync::RwLockReadGuard<'_, Vec<(K, Owned<T>)>> {
        self.entries.read().unwrap_or_else(|p| p.into_inner())
    }

    /// Take the write lock, recovering from poison (a panicking writer
    /// must not wedge the registry; see `crate::lockutil`'s rationale).
    fn with_write(&self, f: impl FnOnce(&mut Vec<(K, Owned<T>)>)) {
        let mut w = self.entries.write().unwrap_or_else(|p| p.into_inner());
        f(&mut w);
    }
}

/// A single owner-tracked slot, for the `set_sink` /
/// `set_local_pane_target` shape where there is ONE install point, not a
/// keyed map (M4/R2).
///
/// Installs stack the way [`OwnedRegistry`] keys do: the latest install
/// is the one in effect, a module's install covers core's, and purging
/// the module brings core's back. A second install by the same owner
/// replaces its first.
pub struct OwnedSlot<T> {
    stack: std::sync::Mutex<Vec<Owned<T>>>,
}

impl<T> Default for OwnedSlot<T> {
    fn default() -> Self {
        Self {
            stack: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl<T> OwnedSlot<T> {
    /// An empty slot, usable in a `static` (the layer stacks in
    /// `layers.rs`).
    pub const fn new() -> Self {
        Self {
            stack: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Every install, bottom to top (core's first, the latest last) —
    /// the whole stack, for a chain that hands each layer the one below
    /// it. `with` reads only the top.
    pub fn all(&self) -> Vec<T>
    where
        T: Clone,
    {
        self.lock().iter().map(|e| e.value.clone()).collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Owned<T>>> {
        self.stack.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn push(&self, item: Owned<T>) {
        let owner = item.owner();
        let mut g = self.lock();
        g.retain(|e| e.owner() != owner);
        g.push(item);
    }

    /// Install `value` for the current thread's owner.
    pub fn set(&self, value: T) {
        self.push(Owned::tracked(value));
    }

    /// Install `value` as core's, regardless of the calling thread.
    pub fn set_core(&self, value: T) {
        self.push(Owned::core(value));
    }

    /// Uninstall what the CALLING owner installed (the `set_sink(None)`
    /// spelling). Another owner's install is not the caller's to remove.
    pub fn clear(&self) {
        let owner = crate::lifecycle::thread_owner();
        self.lock().retain(|e| e.owner() != owner);
    }

    /// Read the install in effect. The closure gets `&T`.
    pub fn with<R>(&self, f: impl FnOnce(&T) -> R) -> Option<R> {
        self.lock().last().map(|e| f(&e.value))
    }

    /// The owner of the install in effect (`Some(None)` = core's).
    pub fn owner(&self) -> Option<Option<&'static str>> {
        self.lock().last().map(|e| e.owner())
    }

    /// Remove `owner`'s install; return whether there was one. What it
    /// covered is in effect again. `None` (core) never matches.
    pub fn purge(&self, owner: Option<&'static str>) -> bool {
        let Some(owner) = owner else {
            return false;
        };
        let mut g = self.lock();
        let before = g.len();
        g.retain(|e| e.owner() != Some(owner));
        g.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `f` with the current thread tagged `owner`, restoring the
    /// previous tag after. The production tag is set by a runtime's
    /// thread-start hook and never cleared; the test needs to scope it so
    /// one case's tag does not leak into the next.
    fn tagged<R>(owner: Option<&'static str>, f: impl FnOnce() -> R) -> R {
        let prev = crate::lifecycle::thread_owner();
        if let Some(o) = owner {
            crate::lifecycle::set_thread_owner(o);
        }
        let r = f();
        crate::lifecycle::clear_thread_owner_for_test(prev);
        r
    }

    /// The basic contract: an entry made on a tagged thread is attributed
    /// to that owner; an entry made untagged is core (None).
    #[test]
    fn insert_attributes_the_calling_threads_owner() {
        let reg: OwnedRegistry<&str, i32> = OwnedRegistry::default();
        reg.insert_core("builtin", 1);
        assert_eq!(reg.owner_of(&"builtin"), Some(None));

        tagged(Some("module:echo"), || {
            reg.insert("echo-tool", 2);
        });
        assert_eq!(reg.owner_of(&"echo-tool"), Some(Some("module:echo")));
    }

    /// THE point of the module: purging one owner removes exactly its
    /// entries and leaves core's and every other module's alone.
    #[test]
    fn purge_removes_exactly_one_owners_entries() {
        let reg: OwnedRegistry<&str, i32> = OwnedRegistry::default();
        reg.insert_core("builtin", 0);
        tagged(Some("module:irc"), || {
            reg.insert("irc-a", 1);
            reg.insert("irc-b", 2);
        });
        tagged(Some("module:telegram"), || {
            reg.insert("tg-a", 3);
        });

        let removed = reg.purge(Some("module:irc"));
        assert_eq!(removed, 2, "both irc entries go");
        assert!(reg.with(&"irc-a", |_| ()).is_none());
        assert!(reg.with(&"irc-b", |_| ()).is_none());
        assert_eq!(reg.with(&"builtin", |v| *v), Some(0), "core survives");
        assert_eq!(reg.with(&"tg-a", |v| *v), Some(3), "telegram survives");
    }

    /// Core is never purgeable: purge(None) is a no-op, not "purge the
    /// core entries".
    #[test]
    fn purge_none_removes_nothing() {
        let reg: OwnedRegistry<&str, i32> = OwnedRegistry::default();
        reg.insert_core("builtin", 1);
        assert_eq!(reg.purge(None), 0);
        assert_eq!(reg.len(), 1);
    }

    /// The reload shape: after a purge, the SAME key re-inserted under a
    /// NEW owner is owned by the new owner — so a reloaded module's
    /// re-registration takes the key over cleanly.
    #[test]
    fn a_reinsert_after_purge_is_owned_by_the_new_owner() {
        let reg: OwnedRegistry<&str, i32> = OwnedRegistry::default();
        tagged(Some("module:echo:v1"), || reg.insert("tool", 1));
        assert_eq!(reg.owner_of(&"tool"), Some(Some("module:echo:v1")));

        reg.purge(Some("module:echo:v1"));
        tagged(Some("module:echo:v2"), || reg.insert("tool", 2));
        assert_eq!(reg.owner_of(&"tool"), Some(Some("module:echo:v2")));
        assert_eq!(reg.with(&"tool", |v| *v), Some(2));
    }

    /// A poisoned write lock does not wedge the registry (a panicking
    /// module must not take the registry down with it).
    #[test]
    fn a_poisoned_lock_still_serves() {
        use std::sync::Arc;
        let reg = Arc::new(OwnedRegistry::<&str, i32>::default());
        let reg2 = Arc::clone(&reg);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            reg2.with_write(|_| panic!("deliberate poison"));
        }));
        // The next write recovers (poison -> into_inner) and works.
        reg.insert_core("still-here", 1);
        assert_eq!(reg.with(&"still-here", |v| *v), Some(1));
    }

    /// A single slot: a module's purge clears ONLY its own install, never
    /// core's, and never another module's.
    #[test]
    fn a_slot_purges_only_its_own_owner() {
        let slot: OwnedSlot<i32> = OwnedSlot::default();
        slot.set_core(1);
        assert!(
            !slot.purge(Some("module:echo")),
            "a module cannot purge core's sink"
        );
        assert_eq!(slot.with(|v| *v), Some(1), "core's sink survives");

        tagged(Some("module:echo"), || slot.set(2));
        assert!(
            !slot.purge(Some("module:other")),
            "the wrong owner purges nothing"
        );
        assert_eq!(slot.with(|v| *v), Some(2));
        assert!(slot.purge(Some("module:echo")), "its own owner purges it");
        assert_eq!(
            slot.with(|v| *v),
            Some(1),
            "the module's install covered core's; core's is back"
        );
        assert!(!slot.purge(None), "core is never purgeable");
        slot.clear();
        assert!(slot.with(|v| *v).is_none(), "core cleared its own");
    }

    /// The registries iterate in registration order (first hook verdict
    /// wins, first session runner that owns a session), and a same-owner
    /// re-registration moves to the end, as `retain` + `push` did.
    #[test]
    fn iteration_is_registration_order_and_a_reregistration_moves_last() {
        let reg: OwnedRegistry<&str, i32> = OwnedRegistry::default();
        for (k, v) in [("c", 3), ("a", 1), ("b", 2)] {
            reg.insert_core(k, v);
        }
        assert_eq!(reg.keys(), vec!["c", "a", "b"]);
        reg.insert_core("c", 30);
        assert_eq!(reg.keys(), vec!["a", "b", "c"]);
        assert_eq!(reg.values_cloned(), vec![1, 2, 30]);
        assert_eq!(reg.len(), 3);
    }

    /// A module overriding a core entry hides it, and unloading the
    /// module brings it back; a module overriding another module's entry
    /// unwinds the same way.
    #[test]
    fn a_purged_override_restores_what_it_covered() {
        let reg: OwnedRegistry<&str, i32> = OwnedRegistry::default();
        reg.insert_core("tool", 0);
        tagged(Some("module:a"), || reg.insert("tool", 1));
        tagged(Some("module:b"), || reg.insert("tool", 2));
        assert_eq!(reg.with(&"tool", |v| *v), Some(2));
        assert_eq!(reg.len(), 1, "one key, however many layers");
        assert_eq!(reg.values_cloned(), vec![2]);

        assert_eq!(reg.purge(Some("module:b")), 1);
        assert_eq!(reg.with(&"tool", |v| *v), Some(1));
        assert_eq!(reg.purge(Some("module:a")), 1);
        assert_eq!(reg.with(&"tool", |v| *v), Some(0));
        assert_eq!(reg.owner_of(&"tool"), Some(None), "core's again");
    }

    /// A poisoned lock does not make the registry look empty.
    #[test]
    fn a_poisoned_lock_still_counts() {
        use std::sync::Arc;
        let reg = Arc::new(OwnedRegistry::<&str, i32>::default());
        reg.insert_core("kept", 1);
        let reg2 = Arc::clone(&reg);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            reg2.with_write(|_| panic!("deliberate poison"));
        }));
        assert_eq!(reg.len(), 1);
        assert!(!reg.is_empty());
        assert_eq!(reg.keys(), vec!["kept"]);
    }
}
