//! The bound on the connections one uid has open at once
//! ([`crate::ServiceOptions::max_connections_per_uid`]).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// The limit of a service whose options do not set one. Half of the
/// connections the service handles at once
/// (`socket_service::MAX_CONCURRENT_CLIENTS`), so one uid whose clients
/// connect and then send nothing leaves the other half to everyone else.
pub(crate) const DEFAULT_MAX_CONNECTIONS_PER_UID: usize = 32;

/// Counts the open connections of each uid and refuses one that would
/// exceed the limit.
///
/// A connection is counted from `accept()` until its descriptor is
/// closed: while it waits for a handler slot, while its request is read
/// and answered, and, for a `ctl.*` request, until the host has answered
/// or dropped the request's `ControlReply`. The [`UidSlot`] that
/// [`admit`](Self::admit) returns stays with the descriptor for that time.
pub(crate) struct UidLimiter {
    limit: usize,
    /// Not counted. A uid that appears here can already stop the service
    /// by other means (uid 0, the service's own uid).
    exempt: Vec<u32>,
    /// The connections each counted uid has open; no entry for zero. The
    /// key `None` is every connection whose credentials could not be read.
    /// Never held across an await.
    open: Mutex<HashMap<Option<u32>, usize>>,
}

impl UidLimiter {
    pub(crate) fn new(limit: usize, exempt: &[u32]) -> Arc<Self> {
        Arc::new(Self {
            limit,
            exempt: exempt.to_vec(),
            open: Mutex::default(),
        })
    }

    /// The most connections a counted uid may have open.
    pub(crate) fn limit(&self) -> usize {
        self.limit
    }

    fn open(&self) -> MutexGuard<'_, HashMap<Option<u32>, usize>> {
        // Nothing can panic while the lock is held, so a poisoned lock
        // would still hold consistent data.
        self.open.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A slot for one more connection of `uid` (`None`: the credentials
    /// could not be read), or `None` when `uid` already has
    /// [`limit`](Self::limit) open.
    pub(crate) fn admit(self: &Arc<Self>, uid: Option<u32>) -> Option<UidSlot> {
        if uid.is_some_and(|uid| self.exempt.contains(&uid)) {
            return Some(UidSlot::uncounted());
        }
        let mut open = self.open();
        let count = open.entry(uid).or_insert(0);
        if *count >= self.limit {
            if *count == 0 {
                open.remove(&uid);
            }
            return None;
        }
        *count += 1;
        Some(UidSlot(Some((Arc::clone(self), uid))))
    }

    /// The connections `uid` has open. An exempt uid has none.
    #[cfg(test)]
    pub(crate) fn open_for(&self, uid: Option<u32>) -> usize {
        self.open().get(&uid).copied().unwrap_or(0)
    }
}

/// One counted connection; dropping it gives the place back. Kept next to
/// the connection's descriptor so that the two end together.
pub(crate) struct UidSlot(Option<(Arc<UidLimiter>, Option<u32>)>);

impl UidSlot {
    /// The slot of a connection that is not counted: one of an exempt uid,
    /// or one that did not come through `accept()` (tests).
    pub(crate) fn uncounted() -> Self {
        Self(None)
    }
}

impl Drop for UidSlot {
    fn drop(&mut self) {
        let Some((limiter, uid)) = self.0.take() else {
            return;
        };
        let mut open = limiter.open();
        if let Some(count) = open.get_mut(&uid) {
            *count -= 1;
            if *count == 0 {
                open.remove(&uid);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_uid_gets_the_limit_and_no_more() {
        let limiter = UidLimiter::new(2, &[]);
        let first = limiter.admit(Some(1000)).expect("first");
        let _second = limiter.admit(Some(1000)).expect("second");
        assert!(limiter.admit(Some(1000)).is_none());
        assert_eq!(limiter.open_for(Some(1000)), 2);

        // Another uid has its own count.
        let _other = limiter.admit(Some(1001)).expect("other uid");

        drop(first);
        assert_eq!(limiter.open_for(Some(1000)), 1);
        let _again = limiter
            .admit(Some(1000))
            .expect("after a slot was given back");
        assert!(limiter.admit(Some(1000)).is_none());
    }

    #[test]
    fn exempt_uids_are_not_counted() {
        let limiter = UidLimiter::new(1, &[0, 973]);
        let slots: Vec<_> = (0..5)
            .map(|_| limiter.admit(Some(973)).expect("exempt"))
            .collect();
        assert_eq!(limiter.open_for(Some(973)), 0);
        assert!(limiter.admit(Some(0)).is_some());
        drop(slots);
        assert_eq!(limiter.open_for(Some(973)), 0);
    }

    #[test]
    fn unknown_credentials_share_one_count() {
        // uid 0 is exempt; a connection without credentials is not taken
        // for it.
        let limiter = UidLimiter::new(1, &[0]);
        let _slot = limiter.admit(None).expect("first");
        assert!(limiter.admit(None).is_none());
        assert_eq!(limiter.open_for(None), 1);
    }

    #[test]
    fn limit_zero_admits_only_exempt_uids() {
        let limiter = UidLimiter::new(0, &[0]);
        assert!(limiter.admit(Some(1000)).is_none());
        // A refusal leaves no entry behind.
        assert!(limiter.open().is_empty());
        assert!(limiter.admit(Some(0)).is_some());
    }

    #[test]
    fn released_uid_leaves_no_entry() {
        let limiter = UidLimiter::new(4, &[]);
        let slots: Vec<_> = (0..3).map(|_| limiter.admit(Some(7)).unwrap()).collect();
        drop(slots);
        assert!(limiter.open().is_empty());
    }
}
