//! Thread rosters: who is in a public thread, for surfaces that have no
//! channel roster of their own (Bluesky: the room is the THREAD).
//!
//! The atproto plugin registers each thread's participant handles here
//! (`thread_room.register`), keyed by the thread root (the chat_id); a
//! consumer that needs `[names]` for a sender on that platform reads
//! them back (the slopdrop module's bridge context, R5). Capped so a
//! busy thread and many threads do not grow it without bound.
//!
//! OWNER-TRACKED (M4/R2): each thread's room records the registering
//! thread's owner, so a module's rooms are purged with it on reload.

use std::sync::OnceLock;

fn thread_rooms() -> &'static crate::owned::OwnedRegistry<String, Vec<String>> {
    static M: OnceLock<crate::owned::OwnedRegistry<String, Vec<String>>> = OnceLock::new();
    M.get_or_init(crate::owned::OwnedRegistry::default)
}

/// The most threads kept at once; the oldest roots are dropped past it.
const THREAD_ROOMS_CAP: usize = 512;
/// The most participant handles kept per thread.
const THREAD_MEMBERS_CAP: usize = 64;

/// Record the participant handles of one thread (the atproto plugin, per
/// mention and after a thread fetch). `members` are bare handles; the
/// caller is merged in by the plugin. Deduped, insertion-ordered. The
/// owner is read from the calling thread (M4).
pub fn register_thread_room(thread_root: &str, members: &[String]) {
    if thread_root.is_empty() {
        return;
    }
    // Merge into any existing member list, then re-insert (which
    // re-attributes the owner to the current thread — a module refreshing
    // its own thread keeps ownership).
    let mut entry = thread_rooms()
        .with(&thread_root.to_string(), |v| v.clone())
        .unwrap_or_default();
    for m in members {
        if !m.is_empty() && !entry.contains(m) && entry.len() < THREAD_MEMBERS_CAP {
            entry.push(m.clone());
        }
    }
    thread_rooms().insert(thread_root.to_string(), entry);
    if thread_rooms().len() > THREAD_ROOMS_CAP {
        // Drop an arbitrary old root (the map is small; a full LRU is not
        // worth it for a 512-entry cache).
        if let Some(k) = thread_rooms().keys().into_iter().next() {
            thread_rooms().remove(&k);
        }
    }
}

/// Remove every thread room `owner` registered (R3 teardown). `None`
/// (core) removes nothing. Returns the count removed.
pub fn purge_owner_thread_rooms(owner: Option<&'static str>) -> usize {
    thread_rooms().purge(owner)
}

/// The participant handles registered for one thread, if any.
pub fn thread_room(thread_root: &str) -> Vec<String> {
    thread_rooms()
        .with(&thread_root.to_string(), |v| v.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_room_dedupes_and_merges_across_registrations() {
        let root = format!("at://rooms-test-{}", std::process::id());
        register_thread_room(&root, &["a.example".into(), "b.example".into()]);
        register_thread_room(
            &root,
            &["b.example".into(), "c.example".into(), String::new()],
        );
        assert_eq!(
            thread_room(&root),
            vec!["a.example", "b.example", "c.example"]
        );
        assert!(thread_room("at://nobody").is_empty());
        register_thread_room("", &["x".into()]);
        assert!(
            thread_room("").is_empty(),
            "an empty root registers nothing"
        );
    }
}
