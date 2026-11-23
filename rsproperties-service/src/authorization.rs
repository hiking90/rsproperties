//! The per-request authorization hook: what a host's
//! [authorizer](crate::ServiceOptions::authorizer) is given
//! ([`SetRequest`], [`PeerInfo`]), what it answers ([`Decision`]), and the
//! call itself ([`Authorization`]).

use std::io;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::sync::Arc;

use rsproperties::PropertyInfoTrie;
use tokio::sync::watch;

use crate::{peer_sockopt, KnownType, Listener};

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
/// what `/proc/<pid>` shows now; [`pidfd`](Self::pidfd) names the process
/// itself.
///
/// [`groups`](Self::groups), [`security_label`](Self::security_label) and
/// [`pidfd`](Self::pidfd) each ask the kernel when they are called (one
/// `getsockopt` on the connection, and a second with the size the kernel
/// asked for when the sender has more than 32 groups or a label that does
/// not fit in 256 bytes), so an authorizer that does not need them pays
/// nothing for them. They borrow the connection, which is why a
/// `PeerInfo` cannot be kept after the authorizer has returned. On a value
/// made by [`for_test`](Self::for_test), which has no connection, they
/// fail with [`io::ErrorKind::Unsupported`].
///
/// Two values are equal when their pid, uid and gid are; the `Debug`
/// output has those three.
#[derive(Clone, Copy)]
pub struct PeerInfo<'a> {
    pid: i32,
    uid: u32,
    gid: u32,
    /// The connection the request came on; `None` in a test value.
    connection: Option<BorrowedFd<'a>>,
}

impl PartialEq for PeerInfo<'_> {
    fn eq(&self, other: &Self) -> bool {
        (self.pid, self.uid, self.gid) == (other.pid, other.uid, other.gid)
    }
}

impl Eq for PeerInfo<'_> {}

impl std::fmt::Debug for PeerInfo<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerInfo")
            .field("pid", &self.pid)
            .field("uid", &self.uid)
            .field("gid", &self.gid)
            .finish()
    }
}

impl<'a> PeerInfo<'a> {
    /// A sender with these credentials, to test an authorizer
    /// ([`SetRequest::for_test`]).
    pub fn for_test(pid: i32, uid: u32, gid: u32) -> PeerInfo<'static> {
        PeerInfo {
            pid,
            uid,
            gid,
            connection: None,
        }
    }

    fn connection(&self) -> io::Result<BorrowedFd<'a>> {
        self.connection.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "a PeerInfo made by for_test has no connection to ask",
            )
        })
    }

    /// The sender's supplementary groups (`SO_PEERGROUPS`, Linux 4.13), as
    /// they were when it called `connect()`. [`gid`](Self::gid), its
    /// effective gid, is in the list only when it is also a supplementary
    /// group. The order is the kernel's.
    ///
    /// # Errors
    ///
    /// `ENOPROTOOPT` on a kernel without the option; any other error of
    /// `getsockopt`.
    pub fn groups(&self) -> io::Result<Vec<u32>> {
        peer_sockopt::peer_groups(self.connection()?)
    }

    /// The sender's security label (`SO_PEERSEC`), as the security module
    /// that labels the socket formats it, without a trailing NUL. Under
    /// SELinux it is the context (`system_u:system_r:init_t:s0`), the
    /// value init's `getpeercon` reads. Other modules (AppArmor, Smack)
    /// have their own format, which may hold more than a profile or label
    /// name and was not measured for this crate: read the value on the
    /// target host before comparing with it.
    ///
    /// # Errors
    ///
    /// `ENOPROTOOPT` when no security module of the kernel labels sockets;
    /// [`io::ErrorKind::InvalidData`] for a label that is not UTF-8; any
    /// other error of `getsockopt`.
    pub fn security_label(&self) -> io::Result<String> {
        let label = peer_sockopt::peer_security_label(self.connection()?)?;
        String::from_utf8(label)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "the label is not UTF-8"))
    }

    /// A new pidfd, close-on-exec, for the process that called `connect()`
    /// (`SO_PEERPIDFD`, Linux 6.5). Unlike [`pid`](Self::pid) it cannot
    /// come to name another process: `/proc/self/fdinfo/<fd>` gives the
    /// pid it names now (`-1` once the process was reaped), and
    /// `pidfd_send_signal` and `waitid(P_PIDFD)` act on that process only.
    ///
    /// The descriptor is the caller's to close; every call returns a new
    /// one.
    ///
    /// # Errors
    ///
    /// `ENOPROTOOPT` on a kernel without the option; `ESRCH` when the
    /// process has already been reaped (kernels before 6.15); any other
    /// error of `getsockopt`.
    pub fn pidfd(&self) -> io::Result<OwnedFd> {
        peer_sockopt::peer_pidfd(self.connection()?)
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
    /// of `SO_PEERCRED`; [`groups`](Self::groups) reads them.
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

/// The request was refused; the reason is logged already. The caller
/// answers `PROP_ERROR_HANDLE_CONTROL_MESSAGE` for `ctl.*`,
/// `PROP_ERROR_PERMISSION_DENIED` otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Refused;

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
    /// name check. `name` must have passed that check. `Err(Refused)` when
    /// it may not. A request that may go on gets `Ok(Some(_))` with the
    /// `property_contexts` type of the name, read with the context in one
    /// lookup: the properties service checks the value against it without
    /// a lookup of its own. For a control request it is the type of
    /// `ctl.<action>$<target>`, which nothing reads. It gets `Ok(None)`
    /// when the type cannot be read; the properties service then looks the
    /// type up and answers as it does without an authorizer.
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
    pub(crate) async fn authorize(
        &mut self,
        name: &str,
        value: &str,
        listener: Listener,
        (pid, uid, gid): (Option<i32>, Option<u32>, Option<u32>),
        connection: Option<BorrowedFd<'_>>,
    ) -> Result<Option<KnownType>, Refused> {
        let (Some(pid), Some(uid), Some(gid)) = (pid, uid, gid) else {
            log::warn!(
                "Refused {name:?} on {listener}: the sender's credentials (SO_PEERCRED) could \
                 not be read"
            );
            return Err(Refused);
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
            return Err(Refused);
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
        // A type that cannot be read is not the access check's to answer:
        // the context is read alone, and an allowed request carries no type,
        // so the properties service looks it up and answers
        // `PROP_ERROR_SET_FAILED`, as without an authorizer.
        let (context, property_type) = match trie.context_and_type(lookup) {
            Ok((context, property_type)) => (context, Some(property_type)),
            Err(_) => match trie.context(lookup) {
                Ok(context) => (context, None),
                Err(e) => {
                    log::warn!(
                        "Refused {name:?} from pid {pid} (uid {uid}, gid {gid}) on {listener}: \
                         cannot read the name's context: {e}"
                    );
                    return Err(Refused);
                }
            },
        };
        let Some(context) = context else {
            log::warn!(
                "Refused {name:?} from pid {pid} (uid {uid}, gid {gid}) on {listener}: \
                 property_contexts gives the name no context"
            );
            return Err(Refused);
        };
        let request = SetRequest {
            name,
            value,
            context,
            listener,
            peer: PeerInfo {
                pid,
                uid,
                gid,
                connection,
            },
        };
        let authorizer = &self.authorizer;
        // The lines below log the value's length, not the value
        // (value-masking policy, see `PropertyMessage`'s Debug). The
        // target of a `ctl.*` request is part of the lookup name above,
        // which `rsproperties` logs at `trace` when an exact
        // `property_contexts` entry matches it (`{name:?}`, so escaped);
        // no other log line of the service has the target.
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| authorizer(&request))) {
            Ok(Decision::Allow) => {
                Ok(property_type.map(|property_type| KnownType(property_type.map(str::to_owned))))
            }
            Ok(Decision::Deny) => {
                log::warn!(
                    "The authorizer denied {name:?} ({} bytes, context {context}) from pid {pid} \
                     (uid {uid}, gid {gid}) on {listener}",
                    value.len()
                );
                Err(Refused)
            }
            Err(_) => {
                log::error!(
                    "The authorizer panicked for {name:?} ({} bytes, context {context}) from pid \
                     {pid} (uid {uid}, gid {gid}) on {listener}; the request is refused",
                    value.len()
                );
                Err(Refused)
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

    impl Authorization {
        /// Whether [`authorize`](Self::authorize) lets the request through.
        pub(crate) async fn allows(
            &mut self,
            name: &str,
            value: &str,
            listener: Listener,
            credentials: (Option<i32>, Option<u32>, Option<u32>),
            connection: Option<BorrowedFd<'_>>,
        ) -> bool {
            self.authorize(name, value, listener, credentials, connection)
                .await
                .is_ok()
        }
    }

    fn trie() -> Arc<PropertyInfoTrie> {
        let entries = [
            PropertyInfoEntry::prefix("vendor.", "u:object_r:vendor_prop:s0", "string").unwrap(),
            PropertyInfoEntry::exact("vendor.int", "u:object_r:vendor_prop:s0", "int").unwrap(),
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
                .allows("vendor.x", "secret", Listener::System, PEER, None)
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

    /// An allowed request carries the type of its name, read in the same
    /// lookup as the context; the default when no entry gives one.
    #[tokio::test]
    async fn allowed_request_carries_the_type_of_its_name() {
        let (mut authorization, sender, _seen) = recording(Decision::Allow);
        sender.send_replace(Some(trie()));
        for (name, property_type) in [
            ("vendor.int", "int"),
            ("vendor.x", "string"),
            ("other.x", "string"),
        ] {
            let known = authorization
                .authorize(name, "1", Listener::Property, PEER, None)
                .await
                .expect("allowed")
                .expect("type read");
            assert_eq!(known.0.as_deref(), Some(property_type), "{name}");
        }
    }

    /// A type that cannot be read does not refuse the request: the
    /// authorizer decides on the context, and an allowed request carries
    /// no type, so the properties service looks it up and answers
    /// `PROP_ERROR_SET_FAILED` as it does without an authorizer.
    #[tokio::test]
    async fn unreadable_type_is_left_to_the_properties_service() {
        let entries = [
            PropertyInfoEntry::exact("vendor.x", "u:object_r:vendor_prop:s0", "double").unwrap(),
        ];
        let mut bytes = build_trie(&entries, "u:object_r:default_prop:s0", "string").unwrap();
        let at = bytes
            .windows(7)
            .position(|window| window == b"double\0")
            .expect("type string in the trie");
        bytes[at] = 0xff;
        let trie = Arc::new(PropertyInfoTrie::new(&bytes).unwrap());
        assert!(trie.context_and_type("vendor.x").is_err());
        let (mut authorization, sender, seen) = recording(Decision::Allow);
        sender.send_replace(Some(trie));
        let known = authorization
            .authorize("vendor.x", "1", Listener::Property, PEER, None)
            .await;
        assert_eq!(known, Ok(None));
        assert_eq!(seen.lock().unwrap()[0].2, "u:object_r:vendor_prop:s0");
    }

    /// A control request's context is that of `ctl.<action>$<target>`.
    #[tokio::test]
    async fn control_request_is_looked_up_with_its_target() {
        let (mut authorization, sender, seen) = recording(Decision::Allow);
        sender.send_replace(Some(trie()));
        for target in ["adbd", "other"] {
            assert!(
                authorization
                    .allows("ctl.start", target, Listener::Property, PEER, None)
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
                    .allows("vendor.x", "1", Listener::Property, peer, None)
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
                .allows("vendor.x", "1", Listener::Property, PEER, None)
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
                .allows("vendor.x", "1", Listener::Property, PEER, None)
                .await
        );

        let (mut authorization, sender, seen) = recording(Decision::Allow);
        drop(sender);
        assert!(
            !authorization
                .allows("vendor.x", "1", Listener::Property, PEER, None)
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
                .allows("vendor.x", "1", Listener::Property, PEER, None)
                .await
        );
        // The next request is still answered.
        assert!(
            !authorization
                .allows("vendor.y", "1", Listener::Property, PEER, None)
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
                .allows("other.x", "1", Listener::Property, PEER, None)
                .await
        );
        assert!(seen.lock().unwrap().is_empty());
        assert!(
            authorization
                .allows("vendor.x", "1", Listener::Property, PEER, None)
                .await
        );
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    /// The accessors ask the connection the call was given: the groups are
    /// those of the process that made the pair, and so is the pidfd.
    #[tokio::test]
    async fn authorizer_can_ask_the_connection_about_its_peer() {
        use std::os::fd::{AsFd, AsRawFd};

        type Asked = (std::io::Result<Vec<u32>>, std::io::Result<i32>, bool);
        let asked: Arc<Mutex<Option<Asked>>> = Arc::default();
        let record = Arc::clone(&asked);
        let (mut authorization, sender) = Authorization::new(Arc::new(move |request| {
            let peer = request.peer();
            let pid = peer.pidfd().map(|pidfd| {
                let info =
                    std::fs::read_to_string(format!("/proc/self/fdinfo/{}", pidfd.as_raw_fd()))
                        .unwrap();
                info.lines()
                    .find_map(|line| line.strip_prefix("Pid:"))
                    .unwrap()
                    .trim()
                    .parse()
                    .unwrap()
            });
            // No LSM that labels sockets on the test host is not a failure.
            let label_ok = match peer.security_label() {
                Ok(label) => !label.contains('\0'),
                Err(e) => e.raw_os_error() == Some(libc::ENOPROTOOPT),
            };
            *record.lock().unwrap() = Some((peer.groups(), pid, label_ok));
            Decision::Allow
        }));
        sender.send_replace(Some(trie()));

        let (server, _client) = std::os::unix::net::UnixStream::pair().unwrap();
        assert!(
            authorization
                .allows(
                    "vendor.x",
                    "1",
                    Listener::Property,
                    PEER,
                    Some(server.as_fd())
                )
                .await
        );
        let (groups, pid, label_ok) = asked.lock().unwrap().take().expect("a call");
        let mut groups = groups.expect("SO_PEERGROUPS");
        groups.sort_unstable();
        let mut own: Vec<u32> = rustix::process::getgroups()
            .unwrap()
            .into_iter()
            .map(|gid| gid.as_raw())
            .collect();
        own.sort_unstable();
        assert_eq!(groups, own);
        assert!(label_ok);
        match pid {
            Ok(pid) => assert_eq!(pid, std::process::id() as i32),
            // Linux before 6.5.
            Err(e) => assert_eq!(e.raw_os_error(), Some(libc::ENOPROTOOPT), "{e}"),
        }
    }

    /// A test value has no connection: the accessors say so, and equality
    /// and `Debug` go by the credentials.
    #[test]
    fn test_peer_has_no_connection() {
        let peer = PeerInfo::for_test(7, 1000, 1001);
        for kind in [
            peer.groups().unwrap_err().kind(),
            peer.security_label().unwrap_err().kind(),
            peer.pidfd().unwrap_err().kind(),
        ] {
            assert_eq!(kind, std::io::ErrorKind::Unsupported);
        }
        assert_eq!(peer, PeerInfo::for_test(7, 1000, 1001));
        assert_ne!(peer, PeerInfo::for_test(7, 1000, 1002));
        assert_eq!(
            format!("{peer:?}"),
            "PeerInfo { pid: 7, uid: 1000, gid: 1001 }"
        );
    }
}
