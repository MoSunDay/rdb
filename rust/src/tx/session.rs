//! Per-connection transaction state: plain data + pure transitions.
//!
//! The state lives in the connection task (`resp::conn`) and is passed by
//! `&mut` into every handler `Ctx`; it is dropped with the connection, so
//! an aborted client takes its MULTI/WATCH state with it (matches Redis:
//! no cross-connection transactions, no server-side cleanup needed).
//!
//! Semantics follow Redis:
//! * `MULTI` opens a queue; commands after it are QUEUED (validation at
//!   queue time, execution at EXEC).
//! * A queue-time rejection (unknown command, blocking command, slot
//!   mismatch, MOVED, nested MULTI, WATCH-inside-MULTI...) marks the
//!   transaction DIRTY: every later command still queues (Redis replies
//!   errors immediately but keeps queueing), and EXEC fails wholesale
//!   with EXECABORT.
//! * WATCH entries record a value-hash per key; EXEC re-hashes under the
//!   latches and aborts (null array) on any change. Any write executed
//!   on the same connection outside MULTI implicitly UNWATCHes.

/// One WATCHed key: the slot prefix it lives under plus a hash of the
/// key's full physical family at WATCH time.
pub struct WatchEntry {
    pub prefix: Vec<u8>,
    pub key: Vec<u8>,
    pub hash: u64,
}

/// Open MULTI state on a connection.
#[derive(Default)]
pub struct MultiState {
    /// Slot every queued key must hash to; bound by the FIRST queued
    /// command that carries keys (keyless commands never bind it).
    pub slot: u16,
    /// Whether `slot` was bound yet (slot 0 is a legal slot value).
    pub slot_bound: bool,
    /// Raw argv (command name included) per queued command, in order.
    pub queued: Vec<Vec<Vec<u8>>>,
    /// Queue-time rejection seen: EXEC must EXECABORT.
    pub dirty: bool,
}

impl MultiState {
    pub fn new() -> MultiState {
        MultiState::default()
    }

    /// All user keys referenced by queued commands (for latching).
    /// Duplicate keys are kept -- callers deduplicate.
    pub fn queued_keys(&self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for argv in &self.queued {
            let first = argv
                .first()
                .map(|v| v.to_ascii_lowercase())
                .unwrap_or_default();
            let args: Vec<Vec<u8>> = argv.iter().skip(1).cloned().collect();
            out.extend(super::keyspec::keys_of(
                &String::from_utf8_lossy(&first),
                &args,
            ));
        }
        out
    }
}

/// Connection-lifetime transaction state (MULTI queue + watches).
#[derive(Default)]
pub struct ConnState {
    pub multi: Option<MultiState>,
    pub watches: Vec<WatchEntry>,
}

/// Result of queuing one command.
pub enum QueueResult {
    Queued,
    /// Transaction marked dirty; the string is the reply text (already
    /// prefixed by the caller's codec error formatting).
    Rejected,
}

impl ConnState {
    pub fn in_multi(&self) -> bool {
        self.multi.is_some()
    }

    pub fn is_dirty(&self) -> bool {
        self.multi.as_ref().is_some_and(|m| m.dirty)
    }

    /// Mark the open transaction dirty (queue-time rejection).
    pub fn mark_dirty(&mut self) {
        if let Some(m) = self.multi.as_mut() {
            m.dirty = true;
        }
    }

    /// Queue one command. `keys` are its user keys (empty for keyless
    /// commands); `slot_of` maps a user key to its hash slot. Fails with
    /// CROSSSLOT when a key hashes away from the transaction's slot.
    pub fn queue(
        &mut self,
        argv: Vec<Vec<u8>>,
        keys: &[Vec<u8>],
        slot_of: &dyn Fn(&[u8]) -> u16,
    ) -> QueueResult {
        let Some(m) = self.multi.as_mut() else {
            return QueueResult::Rejected; // unreachable: caller checked in_multi
        };
        for key in keys {
            let slot = slot_of(key);
            if !m.slot_bound {
                m.slot = slot;
                m.slot_bound = true;
            }
            if slot != m.slot {
                m.dirty = true;
                return QueueResult::Rejected;
            }
        }
        m.queued.push(argv);
        QueueResult::Queued
    }

    /// Leave MULTI (DISCARD, or EXEC finishing); consumes the queue.
    pub fn reset_multi(&mut self) {
        self.multi = None;
    }

    /// Drop all WATCH entries (UNWATCH, EXEC, DISCARD, or an implicit
    /// unwatch caused by a write outside MULTI).
    pub fn clear_watches(&mut self) {
        self.watches.clear();
    }

    /// All user keys referenced by queued commands (for latching).
    /// Duplicate keys are kept -- callers deduplicate.
    pub fn queued_keys(&self) -> Vec<Vec<u8>> {
        self.multi
            .as_ref()
            .map(|m| m.queued_keys())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(k: &[u8]) -> u16 {
        // deterministic stand-in: first byte parity bucket
        (k.first().copied().unwrap_or(0) % 3) as u16
    }

    fn argv(cmd: &str, keys: &[&str]) -> Vec<Vec<u8>> {
        let mut v = vec![cmd.as_bytes().to_vec()];
        v.extend(keys.iter().map(|k| k.as_bytes().to_vec()));
        v
    }

    #[test]
    fn queues_and_binds_slot_on_first_keyed_command() {
        let mut st = ConnState {
            multi: Some(MultiState::new()),
            ..ConnState::default()
        };
        assert!(matches!(
            st.queue(argv("set", &["a", "1"]), &[b"a".to_vec()], &slot),
            QueueResult::Queued
        ));
        assert_eq!(st.multi.as_ref().unwrap().slot, slot(b"a"));
        // same slot: ok
        assert!(matches!(
            st.queue(argv("get", &["a"]), &[b"a".to_vec()], &slot),
            QueueResult::Queued
        ));
        assert_eq!(st.multi.as_ref().unwrap().queued.len(), 2);
        assert!(!st.is_dirty());
    }

    #[test]
    fn crossslot_marks_dirty_and_rejects() {
        let mut st = ConnState {
            multi: Some(MultiState::new()),
            ..ConnState::default()
        };
        st.queue(argv("set", &["a", "1"]), &[b"a".to_vec()], &slot);
        assert!(matches!(
            st.queue(argv("get", &["c"]), &[b"c".to_vec()], &slot),
            QueueResult::Rejected
        ));
        assert!(st.is_dirty());
        // dirty transactions still queue later commands (Redis behavior)
        assert!(matches!(
            st.queue(argv("get", &["a"]), &[b"a".to_vec()], &slot),
            QueueResult::Queued
        ));
    }

    #[test]
    fn keyless_commands_queue_without_touching_slot() {
        let mut st = ConnState {
            multi: Some(MultiState::new()),
            ..ConnState::default()
        };
        assert!(matches!(
            st.queue(argv("ping", &[]), &[], &slot),
            QueueResult::Queued
        ));
        // slot still unbound until a keyed command arrives
        assert!(!st.multi.as_ref().unwrap().slot_bound);
        // a keyed command of ANY slot binds after keyless queues
        assert!(matches!(
            st.queue(argv("get", &["z"]), &[b"z".to_vec()], &slot),
            QueueResult::Queued
        ));
        assert!(st.multi.as_ref().unwrap().slot_bound);
    }

    #[test]
    fn reset_and_watches() {
        let mut st = ConnState {
            multi: Some(MultiState::new()),
            ..ConnState::default()
        };
        st.watches.push(WatchEntry {
            prefix: b"70/".to_vec(),
            key: b"k".to_vec(),
            hash: 42,
        });
        st.reset_multi();
        assert!(!st.in_multi());
        assert_eq!(st.watches.len(), 1);
        st.clear_watches();
        assert!(st.watches.is_empty());
    }

    #[test]
    fn queued_keys_union() {
        let mut st = ConnState {
            multi: Some(MultiState::new()),
            ..ConnState::default()
        };
        st.queue(argv("set", &["a", "1"]), &[b"a".to_vec()], &slot);
        // "a" and "d" share a slot under the test's first-byte%3 stand-in
        st.queue(
            argv("mget", &["a", "d"]),
            &[b"a".to_vec(), b"d".to_vec()],
            &slot,
        );
        let keys = st.queued_keys();
        assert_eq!(keys, vec![b"a".to_vec(), b"a".to_vec(), b"d".to_vec()]);
    }
}
