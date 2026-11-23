//! A rule-based [authorizer](crate::ServiceOptions::authorizer):
//! [`AccessPolicy`] maps names and `property_contexts` contexts to the
//! senders ([`Subjects`]) that may set them.

use std::collections::HashMap;
use std::io;

use crate::{Decision, PeerInfo, SetRequest, POWERCTL_PROPERTY};

/// The senders a rule of an [`AccessPolicy`] allows: uids, gids, or
/// everyone.
///
/// A sender matches when its uid is one of the uids, or when one of the
/// gids is its effective gid or one of its supplementary groups, the
/// groups a file's group permission goes by. The supplementary groups are
/// read from the connection ([`PeerInfo::groups`]) only when the uid and
/// the effective gid did not match. A sender whose groups cannot be read
/// (a kernel before Linux 4.13) is matched by its uid and effective gid
/// alone: a gid rule does not allow it through a supplementary group, and
/// the kernel's error is logged at `debug` level.
///
/// ```
/// use rsproperties_service::Subjects;
///
/// // uid 1000, or a member of gid 3003 or 3004.
/// let subjects = Subjects::new().uid(1000).gid(3003).gid(3004);
/// # let _ = subjects;
/// ```
#[derive(Debug, Clone, Default)]
pub struct Subjects {
    anyone: bool,
    uids: Vec<u32>,
    gids: Vec<u32>,
}

impl Subjects {
    /// No sender. Add some with [`uid`](Self::uid) and [`gid`](Self::gid);
    /// a rule with none allows nobody, which closes a name that a wider
    /// rule or the default would open.
    pub fn new() -> Self {
        Self::default()
    }

    /// Every sender that can connect.
    pub fn anyone() -> Self {
        Self {
            anyone: true,
            ..Self::default()
        }
    }

    /// Also the senders with this effective uid.
    pub fn uid(mut self, uid: u32) -> Self {
        if !self.uids.contains(&uid) {
            self.uids.push(uid);
        }
        self
    }

    /// Also the senders with this effective gid or with this group among
    /// their supplementary groups.
    pub fn gid(mut self, gid: u32) -> Self {
        if !self.gids.contains(&gid) {
            self.gids.push(gid);
        }
        self
    }

    /// Adds what `other` allows.
    fn merge(&mut self, other: Subjects) {
        self.anyone |= other.anyone;
        for uid in other.uids {
            if !self.uids.contains(&uid) {
                self.uids.push(uid);
            }
        }
        for gid in other.gids {
            if !self.gids.contains(&gid) {
                self.gids.push(gid);
            }
        }
    }

    fn allows(&self, peer: &PeerInfo<'_>) -> bool {
        // `PeerInfo::groups` asks the kernel (one `getsockopt`, two for a
        // sender with more than 32 groups).
        self.allows_with(peer.uid(), peer.gid(), || {
            peer.groups().inspect_err(|e| {
                // A test value has no connection to ask: nothing to report.
                if e.kind() != io::ErrorKind::Unsupported {
                    log::debug!(
                        "Cannot read the supplementary groups of pid {} (uid {}): {e}; a gid \
                         rule matches its effective gid only",
                        peer.pid(),
                        peer.uid()
                    );
                }
            })
        })
    }

    /// [`allows`](Self::allows) for a sender with `uid` and effective
    /// `gid`. `groups` reads its supplementary groups; it is called only
    /// for a rule with gids that the uid and the effective gid did not
    /// settle, and its failure leaves the sender unmatched.
    fn allows_with(
        &self,
        uid: u32,
        gid: u32,
        groups: impl FnOnce() -> io::Result<Vec<u32>>,
    ) -> bool {
        if self.anyone || self.uids.contains(&uid) || self.gids.contains(&gid) {
            return true;
        }
        !self.gids.is_empty()
            && groups().is_ok_and(|groups| groups.iter().any(|gid| self.gids.contains(gid)))
    }
}

/// What an [`AccessPolicy`] answers for a request that none of its rules
/// covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unmatched {
    Deny,
    /// Except `ro.` names, `ctl.*` requests and `sys.powerctl`.
    AllowOrdinary,
}

/// An authorizer made of rules: which senders may set which names.
///
/// ```
/// use rsproperties_service::{AccessPolicy, ServiceOptions, Subjects};
///
/// let policy = AccessPolicy::deny_unmatched()
///     // root may do everything.
///     .trust_uid(0)
///     // What `property_contexts` labels `vendor_prop`, except `ro.` names:
///     // members of gid 2000.
///     .allow_context("u:object_r:vendor_prop:s0", Subjects::new().gid(2000))
///     // `debug.*`: everyone.
///     .allow_prefix("debug.", Subjects::anyone())
///     // Starting and stopping `adbd`: uid 2000 (shell).
///     .allow_name("ctl.start$adbd", Subjects::new().uid(2000))
///     .allow_name("ctl.stop$adbd", Subjects::new().uid(2000));
///
/// let options = ServiceOptions::new().authorizer(policy.into_authorizer());
/// # let _ = options;
/// ```
///
/// # How a request is decided
///
/// 1. A sender whose uid was given to [`trust_uid`](Self::trust_uid) is
///    allowed.
/// 2. Otherwise one rule decides, the most specific one that covers the
///    request: the [`allow_name`](Self::allow_name) rule for its name;
///    else the [`allow_prefix`](Self::allow_prefix) rule with the longest
///    prefix of its name; else the [`allow_context`](Self::allow_context)
///    rule for its context. The request is allowed when the sender is one
///    of that rule's [`Subjects`] and denied when it is not: a wider rule
///    that would have allowed the sender is not consulted. So a narrower
///    rule can both open and close what a wider one says.
/// 3. A request that no rule covers is denied by a policy made with
///    [`deny_unmatched`](Self::deny_unmatched) and allowed by one made
///    with [`allow_unmatched`](Self::allow_unmatched).
///
/// Steps 2 and 3 are narrower for a protected request; see below.
///
/// The name a rule is matched against is the request's name, except for a
/// `ctl.*` request, where it is `ctl.<action>$<target>`
/// (`ctl.start$adbd`), the name init looks up in `property_contexts` for
/// it. `allow_prefix("ctl.start$", …)` therefore covers `ctl.start` for
/// every target.
///
/// # Protected requests: `ro.` names, `ctl.*` and `sys.powerctl`
///
/// A request whose name starts with `ro.` or with `ctl.`, or is
/// `sys.powerctl`, is allowed only to a trusted uid or by a rule that
/// names it. The rules that can cover it are:
///
/// - the [`allow_name`](Self::allow_name) rule for its name
///   (`ctl.<action>$<target>` for a `ctl.*` request);
/// - for a `ro.` name, the [`allow_prefix`](Self::allow_prefix) rules
///   whose prefix itself starts with `ro.`; for a `ctl.*` request, those
///   whose prefix starts with `ctl.`. The longest one that is a prefix of
///   the name decides. No prefix rule covers `sys.powerctl`.
///
/// A shorter prefix (the empty prefix, `"ro"`, `"c"`, `"sys."`) and an
/// [`allow_context`](Self::allow_context) rule do not cover a protected
/// request, whatever its context. When none of the rules above covers it,
/// it is denied, by a policy made with
/// [`allow_unmatched`](Self::allow_unmatched) too.
///
/// The service stores the first set of a `ro.` name that has no value, as
/// init does. So unless a uid is trusted or a rule names a `ro.` name or
/// a prefix that starts with `ro.`, no client can fix the value of a
/// `ro.` property that the build.prop files left unset. The build.prop
/// files and the persistent properties do not pass through the
/// authorizer.
///
/// # What a policy does not look at
///
/// The value (other than the target of a `ctl.*` request), the socket the
/// request came in on and the sender's security label. A host that
/// decides by those wraps [`decide`](Self::decide):
///
/// ```
/// use rsproperties_service::{AccessPolicy, Decision, Listener, ServiceOptions};
///
/// let policy = AccessPolicy::deny_unmatched().trust_uid(0);
/// let options = ServiceOptions::new().authorizer(move |request| {
///     if request.listener() == Listener::System {
///         return Decision::Allow;
///     }
///     policy.decide(request)
/// });
/// # let _ = options;
/// ```
#[derive(Debug, Clone)]
pub struct AccessPolicy {
    unmatched: Unmatched,
    trusted_uids: Vec<u32>,
    names: HashMap<String, Subjects>,
    /// Longest prefix first, so the first that matches decides.
    prefixes: Vec<(String, Subjects)>,
    contexts: HashMap<String, Subjects>,
}

impl AccessPolicy {
    fn new(unmatched: Unmatched) -> Self {
        Self {
            unmatched,
            trusted_uids: Vec::new(),
            names: HashMap::new(),
            prefixes: Vec::new(),
            contexts: HashMap::new(),
        }
    }

    /// A policy that denies every request no rule covers. Without rules
    /// and trusted uids it denies everything.
    pub fn deny_unmatched() -> Self {
        Self::new(Unmatched::Deny)
    }

    /// A policy that allows a request no rule covers, unless its name
    /// starts with `ro.` or `ctl.`, or is `sys.powerctl`: such a request
    /// is denied unless the sender's uid is trusted or a rule names it
    /// (see "Protected requests" on [`AccessPolicy`]).
    pub fn allow_unmatched() -> Self {
        Self::new(Unmatched::AllowOrdinary)
    }

    /// Senders with this uid are allowed every request, `ro.` names,
    /// `ctl.*` and `sys.powerctl` included, before any rule is looked at.
    /// Nothing is trusted unless it is named here: neither
    /// uid 0 nor the service's own uid.
    pub fn trust_uid(mut self, uid: u32) -> Self {
        if !self.trusted_uids.contains(&uid) {
            self.trusted_uids.push(uid);
        }
        self
    }

    /// The rule for exactly `name` (`ctl.<action>$<target>` for a control
    /// request). A second rule for the same name adds its subjects to the
    /// first.
    pub fn allow_name(mut self, name: impl Into<String>, subjects: Subjects) -> Self {
        self.names.entry(name.into()).or_default().merge(subjects);
        self
    }

    /// The rule for the names that start with `prefix`. The comparison is
    /// by bytes, with no regard to the dots: `"debug"` also covers
    /// `debugger.x`, so a prefix usually ends with a dot. A second rule
    /// for the same prefix adds its subjects to the first.
    ///
    /// A prefix covers a `ro.` name only when the prefix itself starts
    /// with `ro.`, and a `ctl.*` request (matched as
    /// `ctl.<action>$<target>`) only when it starts with `ctl.`; no
    /// prefix covers `sys.powerctl`. The empty prefix therefore covers
    /// every name but those.
    pub fn allow_prefix(mut self, prefix: impl Into<String>, subjects: Subjects) -> Self {
        let prefix = prefix.into();
        match self.prefixes.iter_mut().find(|(p, _)| *p == prefix) {
            Some((_, existing)) => existing.merge(subjects),
            None => {
                self.prefixes.push((prefix, subjects));
                // Stable: prefixes of one length differ, so at most one of
                // them matches a name.
                self.prefixes
                    .sort_by_key(|(p, _)| std::cmp::Reverse(p.len()));
            }
        }
        self
    }

    /// The rule for the names `property_contexts` gives `context`
    /// ([`SetRequest::context`]), compared as a whole string
    /// (`u:object_r:vendor_prop:s0`). A second rule for the same context
    /// adds its subjects to the first.
    ///
    /// A name that no `property_contexts` entry matches has the default
    /// context, `u:object_r:build_prop:s0`: a rule for that context covers
    /// every such name, and on a host without `property_contexts` files
    /// every name, in both cases apart from the protected ones below.
    ///
    /// A context rule never covers a `ro.` name, a `ctl.*` request or
    /// `sys.powerctl`, whatever context `property_contexts` gives them.
    /// If it did, a rule for the default context would open every
    /// protected name that `property_contexts` does not list, names that
    /// nobody wrote down. Those need an [`allow_name`](Self::allow_name)
    /// rule or an [`allow_prefix`](Self::allow_prefix) rule whose prefix
    /// starts with `ro.` or `ctl.`.
    pub fn allow_context(mut self, context: impl Into<String>, subjects: Subjects) -> Self {
        self.contexts
            .entry(context.into())
            .or_default()
            .merge(subjects);
        self
    }

    /// The decision for `request`; see "How a request is decided" and
    /// "Protected requests" on [`AccessPolicy`].
    pub fn decide(&self, request: &SetRequest<'_>) -> Decision {
        let peer = request.peer();
        if self.trusted_uids.contains(&peer.uid()) {
            return Decision::Allow;
        }
        let name = request.name();
        let is_control = name.starts_with("ctl.");
        // init's name for a control request: `ctl.<action>$<target>`.
        let control_name;
        let matched = if is_control {
            control_name = format!("{name}${}", request.value());
            control_name.as_str()
        } else {
            name
        };
        // A protected request is decided only by a rule that names it:
        // its name rule, or a prefix rule whose prefix itself starts with
        // `ro.` or `ctl.`. A prefix of `matched` that is at least that
        // long starts with it. No prefix rule counts for `sys.powerctl`.
        let min_prefix_len = if is_control {
            "ctl.".len()
        } else if name.starts_with("ro.") {
            "ro.".len()
        } else if name == POWERCTL_PROPERTY {
            usize::MAX
        } else {
            0
        };
        let protected = min_prefix_len > 0;
        let mut rule = self.names.get(matched).or_else(|| {
            self.prefixes
                .iter()
                .find(|(prefix, _)| {
                    prefix.len() >= min_prefix_len && matched.starts_with(prefix.as_str())
                })
                .map(|(_, subjects)| subjects)
        });
        if rule.is_none() && !protected {
            rule = self.contexts.get(request.context());
        }
        let allowed = match rule {
            Some(subjects) => subjects.allows(peer),
            None => !protected && self.unmatched == Unmatched::AllowOrdinary,
        };
        if allowed {
            Decision::Allow
        } else {
            Decision::Deny
        }
    }

    /// The policy as the closure
    /// [`ServiceOptions::authorizer`](crate::ServiceOptions::authorizer)
    /// takes.
    pub fn into_authorizer(self) -> impl Fn(&SetRequest<'_>) -> Decision + Send + Sync + 'static {
        move |request| self.decide(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Listener;

    const VENDOR: &str = "u:object_r:vendor_prop:s0";
    const DEFAULT: &str = "u:object_r:build_prop:s0";

    /// The decision for `name` = `value` with `context`, from uid/gid.
    fn decide(
        policy: &AccessPolicy,
        name: &str,
        value: &str,
        context: &str,
        (uid, gid): (u32, u32),
    ) -> Decision {
        let peer = PeerInfo::for_test(1, uid, gid);
        policy.decide(&SetRequest::for_test(
            name,
            value,
            context,
            Listener::Property,
            peer,
        ))
    }

    const ALICE: (u32, u32) = (1000, 1000);
    const BOB: (u32, u32) = (1001, 2000);

    #[test]
    fn empty_deny_policy_denies_everything() {
        let policy = AccessPolicy::deny_unmatched();
        for name in ["debug.x", "ro.x", "ctl.start", "sys.powerctl"] {
            assert_eq!(decide(&policy, name, "v", DEFAULT, (0, 0)), Decision::Deny);
        }
    }

    #[test]
    fn allow_unmatched_keeps_ro_ctl_and_powerctl_closed() {
        let policy = AccessPolicy::allow_unmatched();
        assert_eq!(
            decide(&policy, "debug.x", "v", DEFAULT, ALICE),
            Decision::Allow
        );
        // Ordinary names: only `ro.`, `ctl.` and the whole name
        // `sys.powerctl` are protected.
        for name in ["rollback.x", "ctlx.y", "ro", "ctl", "sys.powerctl.x"] {
            assert_eq!(
                decide(&policy, name, "v", DEFAULT, ALICE),
                Decision::Allow,
                "{name}"
            );
        }
        for name in ["ro.x", "ctl.start", "sys.powerctl"] {
            assert_eq!(
                decide(&policy, name, "v", DEFAULT, ALICE),
                Decision::Deny,
                "{name}"
            );
        }
        // A rule opens them.
        let policy = policy
            .allow_prefix("ro.boot.", Subjects::new().uid(1000))
            .allow_name("sys.powerctl", Subjects::new().uid(1000))
            .allow_name("ctl.start$adbd", Subjects::new().uid(1000));
        assert_eq!(
            decide(&policy, "ro.boot.x", "v", DEFAULT, ALICE),
            Decision::Allow
        );
        assert_eq!(
            decide(&policy, "ro.other", "v", DEFAULT, ALICE),
            Decision::Deny
        );
        assert_eq!(
            decide(&policy, "sys.powerctl", "reboot", DEFAULT, ALICE),
            Decision::Allow
        );
        assert_eq!(
            decide(&policy, "ctl.start", "adbd", DEFAULT, ALICE),
            Decision::Allow
        );
        assert_eq!(
            decide(&policy, "ctl.start", "adbd2", DEFAULT, ALICE),
            Decision::Deny
        );
        assert_eq!(
            decide(&policy, "sys.powerctl", "reboot", DEFAULT, BOB),
            Decision::Deny
        );
    }

    #[test]
    fn trusted_uid_is_allowed_before_the_rules() {
        let policy = AccessPolicy::deny_unmatched()
            .trust_uid(0)
            .allow_name("vendor.closed", Subjects::new());
        assert_eq!(
            decide(&policy, "vendor.closed", "v", VENDOR, (0, 0)),
            Decision::Allow
        );
        assert_eq!(
            decide(&policy, "ro.x", "v", DEFAULT, (0, 5)),
            Decision::Allow
        );
        // The gid is not the uid.
        assert_eq!(
            decide(&policy, "ro.x", "v", DEFAULT, (5, 0)),
            Decision::Deny
        );
    }

    #[test]
    fn the_most_specific_rule_decides() {
        let policy = AccessPolicy::deny_unmatched()
            .allow_context(VENDOR, Subjects::new().gid(2000))
            .allow_prefix("vendor.", Subjects::new().uid(1000))
            .allow_prefix("vendor.audio.", Subjects::anyone())
            .allow_name("vendor.audio.locked", Subjects::new());

        // The context rule: for a name no name rule covers.
        assert_eq!(
            decide(&policy, "persist.vendor.x", "v", VENDOR, BOB),
            Decision::Allow
        );
        assert_eq!(
            decide(&policy, "persist.vendor.x", "v", VENDOR, ALICE),
            Decision::Deny
        );
        // The prefix rule wins over the context rule, in both directions.
        assert_eq!(
            decide(&policy, "vendor.x", "v", VENDOR, ALICE),
            Decision::Allow
        );
        assert_eq!(
            decide(&policy, "vendor.x", "v", VENDOR, BOB),
            Decision::Deny
        );
        // The longer prefix wins over the shorter.
        assert_eq!(
            decide(&policy, "vendor.audio.x", "v", VENDOR, BOB),
            Decision::Allow
        );
        // The exact name wins over every prefix and closes the name.
        assert_eq!(
            decide(&policy, "vendor.audio.locked", "v", VENDOR, ALICE),
            Decision::Deny
        );
        // No rule: the default.
        assert_eq!(
            decide(&policy, "debug.x", "v", DEFAULT, ALICE),
            Decision::Deny
        );
    }

    /// The order the prefix rules were added in does not matter.
    #[test]
    fn longest_prefix_wins_whatever_the_order() {
        for reversed in [false, true] {
            let mut rules = vec![
                ("a.", Subjects::new()),
                ("a.b.", Subjects::anyone()),
                ("a.b.c.", Subjects::new()),
            ];
            if reversed {
                rules.reverse();
            }
            let mut policy = AccessPolicy::deny_unmatched();
            for (prefix, subjects) in rules {
                policy = policy.allow_prefix(prefix, subjects);
            }
            assert_eq!(decide(&policy, "a.x", "v", DEFAULT, ALICE), Decision::Deny);
            assert_eq!(
                decide(&policy, "a.b.x", "v", DEFAULT, ALICE),
                Decision::Allow
            );
            assert_eq!(
                decide(&policy, "a.b.c.x", "v", DEFAULT, ALICE),
                Decision::Deny
            );
        }
    }

    #[test]
    fn control_requests_match_by_action_and_target() {
        let policy = AccessPolicy::deny_unmatched()
            .allow_prefix("ctl.stop$", Subjects::new().uid(1000))
            .allow_name("ctl.start$adbd", Subjects::new().gid(2000))
            .allow_context("u:object_r:ctl_default_prop:s0", Subjects::new().uid(7));
        assert_eq!(
            decide(&policy, "ctl.stop", "anything", DEFAULT, ALICE),
            Decision::Allow
        );
        assert_eq!(
            decide(&policy, "ctl.start", "adbd", DEFAULT, BOB),
            Decision::Allow
        );
        assert_eq!(
            decide(&policy, "ctl.start", "adbd", DEFAULT, ALICE),
            Decision::Deny
        );
        // A rule for the bare action name does not cover a request.
        let bare = AccessPolicy::deny_unmatched().allow_name("ctl.start", Subjects::anyone());
        assert_eq!(
            decide(&bare, "ctl.start", "adbd", DEFAULT, ALICE),
            Decision::Deny
        );
        // A context rule does not cover a control request, also when the
        // request has that context.
        assert_eq!(
            decide(
                &policy,
                "ctl.restart",
                "x",
                "u:object_r:ctl_default_prop:s0",
                (7, 7)
            ),
            Decision::Deny
        );
        // A target with a `$` in it: everything after the first `$` of
        // the matched name is the target.
        let dollar = AccessPolicy::deny_unmatched().allow_name("ctl.start$a$b", Subjects::anyone());
        assert_eq!(
            decide(&dollar, "ctl.start", "a$b", DEFAULT, ALICE),
            Decision::Allow
        );
        assert_eq!(
            decide(&dollar, "ctl.start", "a", DEFAULT, ALICE),
            Decision::Deny
        );
    }

    /// The three protected requests, as (name, value).
    const PROTECTED: [(&str, &str); 3] = [("ro.x", "v"), ("ctl.start", "x"), ("sys.powerctl", "v")];

    fn both_defaults() -> [AccessPolicy; 2] {
        [
            AccessPolicy::deny_unmatched(),
            AccessPolicy::allow_unmatched(),
        ]
    }

    /// A context rule and a prefix shorter than `ro.` / `ctl.` do not
    /// cover a protected request, in either kind of policy.
    #[test]
    fn protected_requests_are_not_covered_by_context_or_short_prefix_rules() {
        type Rule = fn(AccessPolicy) -> AccessPolicy;
        let rules: [(&str, Rule); 8] = [
            ("default context", |p| {
                p.allow_context(DEFAULT, Subjects::anyone())
            }),
            ("vendor context", |p| {
                p.allow_context(VENDOR, Subjects::anyone())
            }),
            ("prefix ''", |p| p.allow_prefix("", Subjects::anyone())),
            ("prefix 'r'", |p| p.allow_prefix("r", Subjects::anyone())),
            ("prefix 'ro'", |p| p.allow_prefix("ro", Subjects::anyone())),
            ("prefix 'c'", |p| p.allow_prefix("c", Subjects::anyone())),
            ("prefix 'sys.'", |p| {
                p.allow_prefix("sys.", Subjects::anyone())
            }),
            // No prefix rule covers `sys.powerctl`, not even the whole name.
            ("prefix 'sys.powerctl'", |p| {
                p.allow_prefix("sys.powerctl", Subjects::anyone())
            }),
        ];
        for base in both_defaults() {
            for (what, rule) in rules {
                let policy = rule(base.clone());
                for (name, value) in PROTECTED {
                    for context in [DEFAULT, VENDOR] {
                        assert_eq!(
                            decide(&policy, name, value, context, ALICE),
                            Decision::Deny,
                            "{what}: {name} in {context}, {:?}",
                            base.unmatched
                        );
                    }
                }
            }
        }
    }

    /// A name rule, or a prefix rule whose prefix starts with `ro.` or
    /// `ctl.`, covers a protected request, in either kind of policy.
    #[test]
    fn protected_requests_are_covered_by_rules_that_name_them() {
        for base in both_defaults() {
            let policy = base
                .clone()
                .allow_prefix("ro.boot.", Subjects::new().uid(1000))
                .allow_prefix("ctl.start$", Subjects::new().uid(1000))
                .allow_name("sys.powerctl", Subjects::new().uid(1000))
                .allow_name("ctl.stop$adbd", Subjects::new().uid(1000))
                .allow_name("ro.one", Subjects::new().uid(1000));
            for (name, value) in [
                ("ro.boot.x", "v"),
                ("ro.one", "v"),
                ("ctl.start", "x"),
                ("ctl.stop", "adbd"),
                ("sys.powerctl", "reboot"),
            ] {
                assert_eq!(
                    decide(&policy, name, value, DEFAULT, ALICE),
                    Decision::Allow,
                    "{name}"
                );
                // The rule that covers the request decides it.
                assert_eq!(
                    decide(&policy, name, value, DEFAULT, BOB),
                    Decision::Deny,
                    "{name}"
                );
            }
            // What the rules do not name stays closed.
            for (name, value) in [("ro.other", "v"), ("ctl.stop", "x"), ("ctl.restart", "x")] {
                assert_eq!(
                    decide(&policy, name, value, DEFAULT, ALICE),
                    Decision::Deny,
                    "{name}"
                );
            }

            // The shortest prefixes that count: `ro.` and `ctl.`.
            let whole = base
                .clone()
                .allow_prefix("ro.", Subjects::anyone())
                .allow_prefix("ctl.", Subjects::anyone());
            for (name, value) in [("ro.x", "v"), ("ctl.start", "x")] {
                assert_eq!(
                    decide(&whole, name, value, DEFAULT, ALICE),
                    Decision::Allow,
                    "{name}"
                );
            }
            assert_eq!(
                decide(&whole, "sys.powerctl", "v", DEFAULT, ALICE),
                Decision::Deny
            );

            // The longest eligible prefix decides; a shorter prefix that
            // is not eligible neither opens nor closes.
            let nested = base
                .clone()
                .allow_prefix("", Subjects::anyone())
                .allow_prefix("ro.", Subjects::anyone())
                .allow_prefix("ro.secret.", Subjects::new());
            assert_eq!(
                decide(&nested, "ro.x", "v", DEFAULT, ALICE),
                Decision::Allow
            );
            assert_eq!(
                decide(&nested, "ro.secret.x", "v", DEFAULT, ALICE),
                Decision::Deny
            );

            // A trusted uid needs no rule.
            let trusted = base.allow_context(DEFAULT, Subjects::new()).trust_uid(1000);
            for (name, value) in PROTECTED {
                assert_eq!(
                    decide(&trusted, name, value, DEFAULT, ALICE),
                    Decision::Allow,
                    "{name}"
                );
                assert_eq!(
                    decide(&trusted, name, value, DEFAULT, BOB),
                    Decision::Deny,
                    "{name}"
                );
            }
        }
    }

    /// Ordinary names are decided by context rules and by prefixes of any
    /// length.
    #[test]
    fn ordinary_names_are_covered_by_context_and_short_prefix_rules() {
        // The default context: every name without a `property_contexts`
        // entry.
        let policy = AccessPolicy::deny_unmatched().allow_context(DEFAULT, Subjects::anyone());
        for name in ["debug.x", "sys.powerctl.x", "ro", "ctl", "rollback.x"] {
            assert_eq!(
                decide(&policy, name, "v", DEFAULT, ALICE),
                Decision::Allow,
                "{name}"
            );
        }
        assert_eq!(
            decide(&policy, "vendor.x", "v", VENDOR, ALICE),
            Decision::Deny
        );

        // The empty prefix covers every ordinary name.
        let policy = AccessPolicy::deny_unmatched().allow_prefix("", Subjects::anyone());
        for name in ["debug.x", "sys.x", "sys.powerctl.x", "ro", "ctlx.y"] {
            assert_eq!(
                decide(&policy, name, "v", DEFAULT, ALICE),
                Decision::Allow,
                "{name}"
            );
        }

        // A prefix is compared by bytes, with no regard to the dots.
        let policy = AccessPolicy::deny_unmatched().allow_prefix("debug", Subjects::anyone());
        assert_eq!(
            decide(&policy, "debugger.x", "v", DEFAULT, ALICE),
            Decision::Allow
        );
        assert_eq!(decide(&policy, "debu", "v", DEFAULT, ALICE), Decision::Deny);

        // A name rule for everyone.
        let policy = AccessPolicy::deny_unmatched().allow_name("a.b", Subjects::anyone());
        assert_eq!(decide(&policy, "a.b", "v", DEFAULT, BOB), Decision::Allow);
        assert_eq!(decide(&policy, "a.bc", "v", DEFAULT, BOB), Decision::Deny);
    }

    /// The group checks of a rule, without a connection.
    #[test]
    fn subjects_match_uid_gid_and_supplementary_groups() {
        let unreachable = || -> io::Result<Vec<u32>> { panic!("the groups were read") };
        let subjects = Subjects::new().uid(1000).gid(2000);
        // The uid and the effective gid settle it without the groups.
        assert!(subjects.allows_with(1000, 5, unreachable));
        assert!(subjects.allows_with(5, 2000, unreachable));
        // A supplementary group.
        assert!(subjects.allows_with(5, 5, || Ok(vec![7, 2000])));
        assert!(!subjects.allows_with(5, 5, || Ok(vec![7, 1000])));
        assert!(!subjects.allows_with(5, 5, || Ok(Vec::new())));
        // Groups that cannot be read: the sender is not matched.
        assert!(!subjects.allows_with(5, 5, || Err(io::ErrorKind::InvalidInput.into())));
        // A rule without gids does not read them.
        assert!(!Subjects::new().uid(1000).allows_with(5, 5, unreachable));
        assert!(!Subjects::new().allows_with(5, 5, unreachable));
        assert!(Subjects::anyone().allows_with(5, 5, unreachable));
    }

    #[test]
    fn repeated_rules_add_their_subjects() {
        let policy = AccessPolicy::deny_unmatched()
            .allow_name("a", Subjects::new().uid(1000))
            .allow_name("a", Subjects::new().gid(2000))
            .allow_prefix("p.", Subjects::new().uid(1000))
            .allow_prefix("p.", Subjects::new().gid(2000))
            .allow_context(VENDOR, Subjects::new().uid(1000))
            .allow_context(VENDOR, Subjects::new().gid(2000));
        for (name, context) in [("a", DEFAULT), ("p.x", DEFAULT), ("v.x", VENDOR)] {
            assert_eq!(
                decide(&policy, name, "v", context, ALICE),
                Decision::Allow,
                "{name}"
            );
            assert_eq!(
                decide(&policy, name, "v", context, BOB),
                Decision::Allow,
                "{name}"
            );
            assert_eq!(
                decide(&policy, name, "v", context, (5, 5)),
                Decision::Deny,
                "{name}"
            );
        }
    }

    /// A gid rule also matches a supplementary group, read from the
    /// connection; a test value, which has no connection, is matched by
    /// its effective gid alone.
    #[tokio::test]
    async fn gid_rule_matches_a_supplementary_group() {
        use crate::authorization::Authorization;
        use rsproperties::{build_trie, PropertyInfoTrie};
        use std::os::fd::AsFd;
        use std::sync::Arc;

        let egid = rustix::process::getegid().as_raw();
        // A supplementary group of this process besides its egid, when it
        // has one; (gid_t)-1, which is never a group id, when it has none.
        let supplementary = rustix::process::getgroups()
            .unwrap()
            .into_iter()
            .map(|gid| gid.as_raw())
            .find(|gid| *gid != egid);
        let policy = AccessPolicy::deny_unmatched()
            .allow_prefix(
                "in.",
                Subjects::new().gid(supplementary.unwrap_or(u32::MAX)),
            )
            // A gid this process is certainly not in.
            .allow_prefix("out.", Subjects::new().gid(u32::MAX));

        // Without a connection the supplementary groups are unknown.
        for name in ["in.x", "out.x"] {
            assert_eq!(
                decide(&policy, name, "v", DEFAULT, (4242, egid)),
                Decision::Deny,
                "{name}"
            );
        }

        let (mut authorization, sender) =
            Authorization::new(Arc::new(policy.clone().into_authorizer()));
        let bytes = build_trie(&[], DEFAULT, "string").unwrap();
        sender.send_replace(Some(Arc::new(PropertyInfoTrie::new(&bytes).unwrap())));
        let (server, _client) = std::os::unix::net::UnixStream::pair().unwrap();
        // A uid the policy has no rule for, and the real egid.
        let credentials = (Some(1), Some(4242), Some(egid));
        assert!(
            !authorization
                .allows(
                    "out.x",
                    "v",
                    Listener::Property,
                    credentials,
                    Some(server.as_fd())
                )
                .await
        );
        // With a supplementary group the rule for it allows the sender;
        // without one this is the "out." case again.
        if supplementary.is_none() {
            eprintln!("note: no supplementary group besides the egid; only the denial was checked");
        }
        assert_eq!(
            authorization
                .allows(
                    "in.x",
                    "v",
                    Listener::Property,
                    credentials,
                    Some(server.as_fd())
                )
                .await,
            supplementary.is_some()
        );
    }
}
