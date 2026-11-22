//! The per-request authorization hook: what a host's
//! [authorizer](crate::ServiceOptions::authorizer) is given
//! ([`SetRequest`], [`PeerInfo`]), what it answers ([`Decision`]), and the
//! call itself ([`Authorization`]).

use std::marker::PhantomData;
use std::sync::Arc;

use rsproperties::PropertyInfoTrie;
use tokio::sync::watch;

use crate::Listener;

/// What an [authorizer](crate::ServiceOptions::authorizer) answers for one
/// request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum Decision {
    /// The request goes on to the checks and the handling it had without
    /// an authorizer.
    Allow,
    /// The request is refused: `PROP_ERROR_HANDLE_CONTROL_MESSAGE` for a
    /// `ctl.*` request, `PROP_ERROR_PERMISSION_DENIED` for any other, the
    /// codes init sends when `CheckPermissions` fails.
    Deny,
}

/// The sender of a request: the credentials the kernel recorded for the
/// connection (`SO_PEERCRED`).
///
/// They are those of the process that called `connect()`, at that time. A
/// process that was handed the connected descriptor sends with the same
/// credentials, and the pid may have been reused since: do not decide by
/// what `/proc/<pid>` shows now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerInfo<'a> {
    pid: i32,
    uid: u32,
    gid: u32,
    /// For accessors that will read more from the connection, which an
    /// authorizer call borrows.
    connection: PhantomData<&'a ()>,
}

impl PeerInfo<'_> {
    pub(crate) fn new(pid: i32, uid: u32, gid: u32) -> Self {
        Self {
            pid,
            uid,
            gid,
            connection: PhantomData,
        }
    }

    /// A sender with these credentials, to test an authorizer
    /// ([`SetRequest::for_test`]).
    pub fn for_test(pid: i32, uid: u32, gid: u32) -> PeerInfo<'static> {
        PeerInfo::new(pid, uid, gid)
    }

    /// The sender's pid.
    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// The sender's effective uid.
    pub fn uid(&self) -> u32 {
        self.uid
    }

    /// The sender's effective gid. Its supplementary groups are not part
    /// of `SO_PEERCRED`.
    pub fn gid(&self) -> u32 {
        self.gid
    }
}

/// One request from a client, as an
/// [authorizer](crate::ServiceOptions::authorizer) sees it: a set of an
/// ordinary property, a `ctl.*` request, or a `sys.powerctl` set.
///
/// Its `Debug` output does not print the value: it has `<N bytes>` in its
/// place, the form the service's logs use, and the other fields in full.
#[derive(Clone, Copy)]
pub struct SetRequest<'a> {
    name: &'a str,
    value: &'a str,
    context: &'a str,
    listener: Listener,
    peer: PeerInfo<'a>,
}

// The value is masked as in `PropertyMessage`'s `Debug`: an authorizer
// that logs a request with `{:?}` must not write the value to the log.
impl std::fmt::Debug for SetRequest<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SetRequest")
            .field("name", &self.name)
            .field("value", &format_args!("<{} bytes>", self.value.len()))
            .field("context", &self.context)
            .field("listener", &self.listener)
            .field("peer", &self.peer)
            .finish()
    }
}

impl<'a> SetRequest<'a> {
    /// A request with these parts, to test an authorizer.
    ///
    /// ```
    /// use rsproperties_service::{Decision, Listener, PeerInfo, SetRequest};
    ///
    /// let authorizer = |request: &SetRequest<'_>| {
    ///     if request.peer().uid() == 0 || request.name().starts_with("debug.") {
    ///         Decision::Allow
    ///     } else {
    ///         Decision::Deny
    ///     }
    /// };
    ///
    /// let peer = PeerInfo::for_test(4242, 1000, 1000);
    /// let context = "u:object_r:default_prop:s0";
    /// let debug = SetRequest::for_test("debug.x", "1", context, Listener::Property, peer);
    /// let other = SetRequest::for_test("vendor.x", "1", context, Listener::Property, peer);
    /// assert_eq!(authorizer(&debug), Decision::Allow);
    /// assert_eq!(authorizer(&other), Decision::Deny);
    /// ```
    pub fn for_test(
        name: &'a str,
        value: &'a str,
        context: &'a str,
        listener: Listener,
        peer: PeerInfo<'a>,
    ) -> Self {
        Self {
            name,
            value,
            context,
            listener,
            peer,
        }
    }

    /// The name as the client sent it; `ctl.<action>` for a control
    /// request. It has passed the name check.
    pub fn name(&self) -> &'a str {
        self.name
    }

    /// The value as the client sent it; the target of a control request,
    /// the command of a `sys.powerctl` set. The service does not log
    /// values, with one exception: the target of a control request is
    /// printed in a `trace` line of `rsproperties` when `property_contexts`
    /// has an exact entry for `ctl.<action>$<target>` ("Value masking" in
    /// `docs/service-security.md`). The authorizer gets the value in full;
    /// the `Debug` output of the request does not print it. It has not
    /// been checked against the property's type yet.
    pub fn value(&self) -> &'a str {
        self.value
    }

    /// The context `property_contexts` gives the name
    /// (`u:object_r:vendor_prop:s0`), which init passes to SELinux as the
    /// target of the check. For a control request it is the context of
    /// `ctl.<action>$<target>`, the name init looks up for it.
    ///
    /// A name that no `property_contexts` entry matches has the context
    /// `u:object_r:build_prop:s0`, the default the service builds its
    /// `property_info` with: such a request is not refused by the service,
    /// so an authorizer that allows that context allows every name
    /// without an entry.
    pub fn context(&self) -> &'a str {
        self.context
    }

    /// The socket the request came in on.
    pub fn listener(&self) -> Listener {
        self.listener
    }

    /// The sender.
    pub fn peer(&self) -> &PeerInfo<'a> {
        &self.peer
    }
}

/// The stored form of an authorizer (see
/// [`ServiceOptions::authorizer`](crate::ServiceOptions::authorizer)).
pub(crate) type Authorizer = Arc<dyn Fn(&SetRequest<'_>) -> Decision + Send + Sync + 'static>;

/// The trie the properties service publishes as `property_info`, once its
/// `on_start` has built it. `None` until then.
pub(crate) type TrieReceiver = watch::Receiver<Option<Arc<PropertyInfoTrie>>>;
/// The sending half of [`TrieReceiver`], owned by the properties service's
/// `on_start`.
pub(crate) type TrieSender = watch::Sender<Option<Arc<PropertyInfoTrie>>>;

/// The authorizer and what a call of it needs. Each connection task has a
/// clone.
#[derive(Clone)]
pub(crate) struct Authorization {
    authorizer: Authorizer,
    trie: TrieReceiver,
}

impl Authorization {
    /// `authorizer`, and the two ends of the channel the trie comes
    /// through.
    pub(crate) fn new(authorizer: Authorizer) -> (Self, TrieSender) {
        let (sender, trie) = watch::channel(None);
        (Self { authorizer, trie }, sender)
    }

    /// Whether the request may go on: init's `CheckPermissions` after its
    /// name check. `name` must have passed that check.
    ///
    /// The request is refused without a call when the sender's credentials
    /// could not be read, when the properties service failed to start, or
    /// when the name's context cannot be read (the trie the service builds
    /// has a default context, so every name has one there; a trie without
    /// a default gives a name without an entry none). It is refused too
    /// when the authorizer panics: a defect in the policy must not read as
    /// "allowed".
    ///
    /// Waits for the properties service's `on_start` when it has not
    /// built the trie yet. The caller's deadline bounds the wait.
    pub(crate) async fn allows(
        &mut self,
        name: &str,
        value: &str,
        listener: Listener,
        (pid, uid, gid): (Option<i32>, Option<u32>, Option<u32>),
    ) -> bool {
        let (Some(pid), Some(uid), Some(gid)) = (pid, uid, gid) else {
            log::warn!(
                "Refused {name:?} on {listener}: the sender's credentials (SO_PEERCRED) could \
                 not be read"
            );
            return false;
        };
        let trie = match self.trie.wait_for(Option::is_some).await {
            // `wait_for` returned for a `Some`.
            Ok(trie) => trie.as_ref().map(Arc::clone),
            Err(_) => None,
        };
        let Some(trie) = trie else {
            log::warn!(
                "Refused {name:?} from pid {pid} (uid {uid}, gid {gid}) on {listener}: the \
                 properties service did not start, so the name's context is unknown"
            );
            return false;
        };
        // init looks a control request up as `ctl.<action>$<target>`
        // (`CheckControlPropertyPerms`); the legacy `ctl.<target>` lookup
        // it tries first for start, stop and restart is not made.
        let control_name;
        let lookup = if name.starts_with("ctl.") {
            control_name = format!("{name}${value}");
            control_name.as_str()
        } else {
            name
        };
        let context = match trie.context(lookup) {
            Ok(Some(context)) => context,
            Ok(None) => {
                log::warn!(
                    "Refused {name:?} from pid {pid} (uid {uid}, gid {gid}) on {listener}: \
                     property_contexts gives the name no context"
                );
                return false;
            }
            Err(e) => {
                log::warn!(
                    "Refused {name:?} from pid {pid} (uid {uid}, gid {gid}) on {listener}: \
                     cannot read the name's context: {e}"
                );
                return false;
            }
        };
        let request = SetRequest {
            name,
            value,
            context,
            listener,
            peer: PeerInfo::new(pid, uid, gid),
        };
        let authorizer = &self.authorizer;
        // The lines below log the value's length, not the value
        // (value-masking policy, see `PropertyMessage`'s Debug). The
        // target of a `ctl.*` request is part of the lookup name above,
        // which `rsproperties` logs at `trace` when an exact
        // `property_contexts` entry matches it (`{name:?}`, so escaped);
        // no other log line of the service has the target.
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| authorizer(&request))) {
            Ok(Decision::Allow) => true,
            Ok(Decision::Deny) => {
                log::warn!(
                    "The authorizer denied {name:?} ({} bytes, context {context}) from pid {pid} \
                     (uid {uid}, gid {gid}) on {listener}",
                    value.len()
                );
                false
            }
            Err(_) => {
                log::error!(
                    "The authorizer panicked for {name:?} ({} bytes, context {context}) from pid \
                     {pid} (uid {uid}, gid {gid}) on {listener}; the request is refused",
                    value.len()
                );
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsproperties::{build_trie, PropertyInfoEntry};
    use std::sync::Mutex;

    const PEER: (Option<i32>, Option<u32>, Option<u32>) = (Some(7), Some(1000), Some(1001));

    fn trie() -> Arc<PropertyInfoTrie> {
        let entries = [
            PropertyInfoEntry::prefix("vendor.", "u:object_r:vendor_prop:s0", "string").unwrap(),
            PropertyInfoEntry::exact("ctl.start$adbd", "u:object_r:ctl_adbd_prop:s0", "string")
                .unwrap(),
        ];
        let bytes = build_trie(&entries, "u:object_r:default_prop:s0", "string").unwrap();
        Arc::new(PropertyInfoTrie::new(&bytes).unwrap())
    }

    /// What the authorizer was called with, as owned values.
    type Seen = Arc<Mutex<Vec<(String, String, String, Listener, (i32, u32, u32))>>>;

    fn recording(decision: Decision) -> (Authorization, TrieSender, Seen) {
        let seen = Seen::default();
        let record = Arc::clone(&seen);
        let (authorization, sender) = Authorization::new(Arc::new(move |request| {
            let peer = request.peer();
            record.lock().unwrap().push((
                request.name().to_owned(),
                request.value().to_owned(),
                request.context().to_owned(),
                request.listener(),
                (peer.pid(), peer.uid(), peer.gid()),
            ));
            decision
        }));
        (authorization, sender, seen)
    }

    #[tokio::test]
    async fn authorizer_sees_the_request_and_decides() {
        for decision in [Decision::Allow, Decision::Deny] {
            let (mut authorization, sender, seen) = recording(decision);
            sender.send_replace(Some(trie()));
            let allowed = authorization
                .allows("vendor.x", "secret", Listener::System, PEER)
                .await;
            assert_eq!(allowed, decision == Decision::Allow);
            assert_eq!(
                *seen.lock().unwrap(),
                [(
                    "vendor.x".to_owned(),
                    "secret".to_owned(),
                    "u:object_r:vendor_prop:s0".to_owned(),
                    Listener::System,
                    (7, 1000, 1001)
                )]
            );
        }
    }

    /// A control request's context is that of `ctl.<action>$<target>`.
    #[tokio::test]
    async fn control_request_is_looked_up_with_its_target() {
        let (mut authorization, sender, seen) = recording(Decision::Allow);
        sender.send_replace(Some(trie()));
        for target in ["adbd", "other"] {
            assert!(
                authorization
                    .allows("ctl.start", target, Listener::Property, PEER)
                    .await
            );
        }
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].0, "ctl.start");
        assert_eq!(seen[0].1, "adbd");
        assert_eq!(seen[0].2, "u:object_r:ctl_adbd_prop:s0");
        assert_eq!(seen[1].2, "u:object_r:default_prop:s0");
    }

    /// `Debug` must not print the value (value-masking policy); the other
    /// fields are printed.
    #[test]
    fn debug_masks_the_value() {
        let request = SetRequest::for_test(
            "vendor.x",
            "secret-token",
            "u:object_r:vendor_prop:s0",
            Listener::System,
            PeerInfo::for_test(7, 1000, 1001),
        );
        for debug in [format!("{request:?}"), format!("{request:#?}")] {
            assert!(!debug.contains("secret-token"), "value leaked: {debug}");
            assert!(debug.contains("<12 bytes>"), "length missing: {debug}");
            assert!(debug.contains("vendor.x"), "name missing: {debug}");
            assert!(
                debug.contains("u:object_r:vendor_prop:s0"),
                "context missing: {debug}"
            );
            assert!(debug.contains("System"), "listener missing: {debug}");
            assert!(debug.contains("1000"), "peer missing: {debug}");
        }
    }

    #[tokio::test]
    async fn unknown_credentials_are_refused_without_a_call() {
        let (mut authorization, sender, seen) = recording(Decision::Allow);
        sender.send_replace(Some(trie()));
        for peer in [
            (None, None, None),
            (None, Some(0), Some(0)),
            (Some(1), None, Some(0)),
            (Some(1), Some(0), None),
        ] {
            assert!(
                !authorization
                    .allows("vendor.x", "1", Listener::Property, peer)
                    .await
            );
        }
        assert!(seen.lock().unwrap().is_empty());
    }

    /// A request that arrives before the properties service has built the
    /// trie waits for it; when that service fails to start (its sender is
    /// dropped), the request is refused without a call.
    #[tokio::test]
    async fn waits_for_the_trie_and_refuses_when_it_never_comes() {
        let (mut authorization, sender, seen) = recording(Decision::Allow);
        let mut waiting = authorization.clone();
        let waiter = tokio::spawn(async move {
            waiting
                .allows("vendor.x", "1", Listener::Property, PEER)
                .await
        });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        sender.send_replace(Some(trie()));
        assert!(waiter.await.unwrap());
        assert_eq!(seen.lock().unwrap().len(), 1);

        // The value stays readable after the sender is gone.
        drop(sender);
        assert!(
            authorization
                .allows("vendor.x", "1", Listener::Property, PEER)
                .await
        );

        let (mut authorization, sender, seen) = recording(Decision::Allow);
        drop(sender);
        assert!(
            !authorization
                .allows("vendor.x", "1", Listener::Property, PEER)
                .await
        );
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn panicking_authorizer_refuses() {
        let (mut authorization, sender) = Authorization::new(Arc::new(|_| panic!("policy defect")));
        sender.send_replace(Some(trie()));
        assert!(
            !authorization
                .allows("vendor.x", "1", Listener::Property, PEER)
                .await
        );
        // The next request is still answered.
        assert!(
            !authorization
                .allows("vendor.y", "1", Listener::Property, PEER)
                .await
        );
    }

    /// A trie without a default context gives a name without an entry no
    /// context: the request is refused without a call.
    #[tokio::test]
    async fn name_without_a_context_is_refused() {
        let entries = [
            PropertyInfoEntry::prefix("vendor.", "u:object_r:vendor_prop:s0", "string").unwrap(),
        ];
        // An empty default is "no default" in the trie.
        let bytes = build_trie(&entries, "", "").unwrap();
        let (mut authorization, sender, seen) = recording(Decision::Allow);
        sender.send_replace(Some(Arc::new(PropertyInfoTrie::new(&bytes).unwrap())));
        assert!(
            !authorization
                .allows("other.x", "1", Listener::Property, PEER)
                .await
        );
        assert!(seen.lock().unwrap().is_empty());
        assert!(
            authorization
                .allows("vendor.x", "1", Listener::Property, PEER)
                .await
        );
        assert_eq!(seen.lock().unwrap().len(), 1);
    }
}
